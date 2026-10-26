# Floodgate plugin for Infrarust (MVP)

This plugin lets a Bedrock player who connects through **Geyser Standalone + Floodgate** join a
Java server behind **Infrarust** in `client_only` mode. It has two parts:

- `src/` (`infrarust-plugin-floodgate`): a pure Rust codec for the Floodgate handshake marker and
  the Bedrock identity model. Native and WASM builds share this code.
- `wasm/` (`infrarust-plugin-floodgate-wasm`): the standalone WASM adapter that Infrarust loads.
  It checks the Geyser source, decodes the marker, and admits the verified Bedrock login.

Target: Infrarust `2.0.0-beta.4` (workspace `version`), with no changes to the proxy API.

## Evidence status

Items marked "lead re-ran" were run by the lead for this revision. Everything else is **NOT RUN**.

| Evidence | Result |
|---|---|
| Core codec/identity tests (`infrarust-plugin-floodgate`) | **36 passed** (lead re-ran) |
| Synthetic fixture: Python AES-GCM–generated payload decoded by the Rust codec | **PASS** (lead re-ran). The fixture is synthetic, not a live Geyser capture |
| Parser boundary: beta.4 `SHandshake` parser with the fixture host | **PASS** (lead re-ran). A host of exactly 255 characters parses; 256 fails |
| WASM adapter `clippy -D warnings` (`wasm32-wasip2`) | **Passed** (lead re-ran) |
| Release `wasm32-wasip2` component build | **Passed** (lead re-ran) |
| Handshake length cap | **Measured** at 255 characters (see "Handshake length cap"). Config-solvable with a short hostname; no core change |
| Real Geyser Standalone + Floodgate run | **NOT RUN** |
| Real Bedrock client login end to end | **NOT RUN** |
| Real Fabric backend behind the proxy | **NOT RUN** |
| Capture from a real Floodgate/Geyser handshake (codec compatibility) | **NOT RUN**: no real upstream capture |
| Firewall, key-mount, and log-redaction checks | **NOT RUN** |

Source references below point to the code as it stands in this checkout.

## Topology

```
Bedrock client --UDP--> Geyser Standalone --TCP (Java)--> Infrarust (client_only) --TCP--> Fabric backend
                        (Floodgate auth)                   + floodgate.wasm              (online-mode=false)
```

- No Velocity is required. There is no player-forwarding setup in this MVP.
- Java players keep Mojang online auth. The plugin only admits a login it has verified itself.

## Requirements

| Component | Requirement |
|---|---|
| Infrarust | `2.0.0-beta.4` |
| Geyser | Standalone with Floodgate auth. The example keys match Geyser's current v4 default sample; verify against the exact version you run (see the example) |
| Backend | Fabric with `online-mode=false`. `client_only` requires an offline backend |
| Build | Rust `1.99.0` with the `wasm32-wasip2` target (`rust-version = "1.99"` in the workspace; CI pins `1.99.0`) |

## Files

| File | Purpose |
|---|---|
| `examples/infrarust/infrarust.toml` | Proxy config: `receive_proxy_protocol = false`, plugin grant, key mount |
| `examples/infrarust/servers/fabric-bedrock.toml` | Fabric server entry with `client_only` and `domain_rewrite` |
| `examples/infrarust/plugin-config.toml.example` | Plugin settings file, installed beside the `.wasm` (see below) |
| `examples/geyser/config.yml` | Geyser Standalone config checked against the current v4 default sample |

## Install layout

Infrarust loads every `.wasm` directly in `plugins_dir` (subdirectories are not scanned). Plugin
settings live in the plugin's own data directory, `<plugins_dir>/floodgate/`:

```
plugins/
  floodgate.wasm              # built adapter; the name follows the example comment
  floodgate/
    config.toml               # copied from examples/infrarust/plugin-config.toml.example
```

The adapter reads `config.toml` from its data directory. Its `key_file` is a path inside the guest.
The mount in `infrarust.toml` provides that path.

## Plugin settings (`<plugins_dir>/floodgate/config.toml`)

Keys read by the adapter (`wasm/src/lib.rs`, `deny_unknown_fields`):

| Key | Default | Meaning |
|---|---|---|
| `key_file` | required | Guest path of the 16-byte key, for example `/keys/key.pem` |
| `trusted_sources` | required, at least one | CIDRs allowed to send Floodgate handshakes. Only Geyser's TCP peer address |
| `username_prefix` | `"."` | Prepended to the Bedrock username. Must be shorter than 16 characters |
| `replace_spaces` | `true` | Replaces spaces in the Bedrock name with `_` |
| `pending_ttl_seconds` | `30` | How long a verified handshake waits for its PreLogin (1 to 3600) |
| `replay_ttl_seconds` | `3600` | How long a seen IV is remembered (1 to 3600) |
| `max_pending` | `4096` | Pending-login cache size (1 to 65536) |
| `max_replays` | `16384` | Replay-cache size (1 to 65536) |

The adapter refuses to load if `trusted_sources` is empty, a TTL or capacity is out of range, the
key file is not exactly 16 bytes, or the prefix is 16 characters or longer.

## Key handling

The key is a shared secret. Geyser uses it to write the handshake marker, and the adapter uses the
same key to decrypt it.

- The key is **16 raw bytes**. The adapter rejects any other length at load time.
- Keep it outside the repository. Never commit it and never paste it into logs, issues or CI output.
  The root `.gitignore` ignores `**/key.pem` and `/plugins/floodgate/config.toml`, so a key file
  named `key.pem` is excluded from git wherever it sits in the tree. Other key file names are not covered.
- **Geyser side**: `floodgate-key-file` in Geyser's config points at the key. Restrict the file to the
  Geyser user (owner read only).
- **Infrarust side**: mount a directory that contains only the key, read-only, at `/keys`:

  ```toml
  [plugins.floodgate]
  permissions = ["filesystem-extended"]

  [[plugins.floodgate.wasm.mounts]]
  host = "<HOST_KEY_DIR_OUTSIDE_REPO>"   # directory containing only key.pem
  guest = "/keys"
  read_only = true
  ```

  The mount is declared in `[[plugins.<id>.wasm.mounts]]`, with `read_only` defaulting to `true`
  (`crates/infrarust_config/src/types/wasm_network.rs`). `filesystem-extended` is the capability
  that grants it (`docs/v2/plugins/wasm/capabilities.md`).
- **Minimum permissions**: the adapter source needs only the baseline capabilities plus
  `filesystem-extended` for the key. It does not use `network`, so do not grant it. Not tested with
  `strict_capabilities`.
- Key-file permissions are written for Linux. They are **NOT RUN** on the target hosts.

## PROXY protocol and source trust

- Set `receive_proxy_protocol = false` in `infrarust.toml`. The adapter refuses to load when it is
  `true` (`wasm/src/lib.rs`, `on_enable`). The reason: with PROXY protocol enabled, the event
  `remote_addr` is taken from the header, which a client can spoof. The adapter therefore does not
  use it for correlation or trust.
- `trusted_sources` is the only trust gate. It must hold the **TCP peer address that Infrarust sees
  from Geyser**. If Geyser sits behind NAT, use the NAT egress address.
- Infrarust has no per-source allow-list for the Java port. Enforce it with firewall rules on the
  private network: allow TCP to Infrarust's Java port only from Geyser's address, and deny the rest.
  The firewall rules are host-specific and are not included here. **NOT RUN.**
- Keep Infrarust's `bind` on the private overlay or service interface, not a public address.

## Replay limits

- Each handshake carries a 12-byte IV. The adapter remembers every IV for `replay_ttl_seconds`
  (default 3600) and rejects a second use of the same IV inside that window.
- When the replay cache is full (`max_replays`), new Floodgate logins are rejected. The adapter fails
  closed.
- The cache is in memory. It resets when the proxy restarts, so a handshake captured before a restart
  can be accepted again. **NOT RUN.**
- A replay after the TTL expires is accepted. This is not freshness: the adapter does not check a
  timestamp.

## Domain rewrite: keep the Floodgate blob away from the backend

Geyser adds the Floodgate marker to the handshake hostname after a NUL byte. The adapter reads the
marker from `raw_host`. Routing uses the host part before the first NUL (`util/mod.rs`, and the
`claim.rs` tests).

What each `domain_rewrite` setting does to the handshake forwarded to the backend, from reading
`crates/infrarust-core/src/util/domain_rewrite.rs`:

| Setting | Forwarded handshake host | Marker reaches backend? |
|---|---|---|
| `"none"` (default) | original bytes, unchanged | **Yes** |
| `"from_backend"` | first backend address, Forge markers kept | No |
| `{ explicit = "..." }` | the explicit value, Forge markers kept | No |

Use `from_backend` or `{ explicit = "..." }`. Both rebuild the host and keep only Forge suffixes,
which drops the Floodgate marker. This is read from the source and **NOT RUN** against a backend.
The order in which the adapter reads `raw_host` relative to the rewrite has also not been run.

## Java passthrough

- A handshake with no Floodgate marker is not touched. Java players go through normal Mojang auth.
- The adapter forces offline mode only for a socket whose handshake it verified. For any other
  socket, a `ForceOffline` result from another plugin is denied.
- At `GameProfileRequest`, a socket with no verified record in offline mode is denied. Java online
  auth therefore stays in force.
- **NOT RUN** with a real Java account.

## Identity: name and UUID

From `src/identity.rs` and `wasm/src/lib.rs`:

- **UUID**: `UUID(0, xuid)`, where `xuid` is the Xbox user ID from the marker. This is not the
  vanilla offline UUID, so do not expect the `nameUUIDFromBytes` value for Bedrock players.
- **Java name**: `username_prefix` followed by the Bedrock name, with spaces replaced by `_` when
  `replace_spaces = true`, truncated to 16 characters. With the default prefix `.`, the Bedrock name
  `Steve Player` becomes `.Steve_Player`.
- **Name check**: at PreLogin the adapter requires the login name to equal the Bedrock name, the
  Java name, or the Bedrock name with spaces replaced by `_`. Otherwise it denies the login.
- **Properties**: the profile's properties are cleared, so no Mojang textures are attached to a
  Bedrock identity.
- **Linked accounts**: a marker whose link field is not `null` is rejected
  (`LinkedPlayerUnsupported`).
- **IP**: the marker also carries a client IP. The adapter does not apply it. The player's address
  stays the **Geyser TCP peer address**.

Name collisions with Java players are not handled. Check the prefix against your Java namespace.

## Limitations

- **No real Bedrock or Geyser run has been done.** The integration is unverified end to end.
- **No real upstream capture.** The codec follows the framing documented in `src/codec.rs`
  (`^Floodgate^>`, base64 IV, `!`, base64 ciphertext with the GCM tag, AES-128-GCM, no AAD). The
  only decode test uses a **synthetic** Python AES-GCM fixture. It is not checked against bytes
  captured from a real Floodgate/Geyser handshake.
- **Player IP is the Geyser transport IP.** IP bans, rate limits and filters apply to Geyser's
  address, so Bedrock players share one IP for these checks.
- **No linked accounts.**
- **No skins.** Properties are cleared.
- **No backend Floodgate API.** The Fabric backend does not run Floodgate.
- **Replay is not freshness.** See "Replay limits".
- **Handshake length cap**: measured. The beta.4 `SHandshake` parser accepts a hostname of exactly
  255 characters and rejects 256. A Floodgate marker is long, so the hostname part must stay short
  (see "Handshake length cap"). This is a config constraint, not a core change.

## Build and test

The adapter is a standalone crate with its own `[workspace]`, so its target directory is local to
`wasm/`. Commands use the `1.99.0` toolchain pinned by CI.

One-time setup:

```sh
rustup toolchain install 1.99.0 --profile minimal --component clippy,rustfmt --target wasm32-wasip2
```

Format, lint and build the WASM adapter:

```sh
cargo +1.99.0 fmt --manifest-path plugins/infrarust-plugin-floodgate/wasm/Cargo.toml -- --check
cargo +1.99.0 clippy --manifest-path plugins/infrarust-plugin-floodgate/wasm/Cargo.toml --target wasm32-wasip2 -- -D warnings
cargo +1.99.0 build --manifest-path plugins/infrarust-plugin-floodgate/wasm/Cargo.toml --target wasm32-wasip2 --release
```

The release artifact is `plugins/infrarust-plugin-floodgate/wasm/target/wasm32-wasip2/release/infrarust_plugin_floodgate_wasm.wasm`.
Rename or copy it to `<plugins_dir>/floodgate.wasm`.

Core codec tests for the shared crate:

```sh
cargo +1.99.0 test -p infrarust-plugin-floodgate
```

Run from the repository root. The lead re-ran this command for this revision: **36 tests passed**,
including the synthetic fixture decode and the parser boundary test.

## Handshake length cap

- The beta.4 `SHandshake` parser accepts a `server_address` of up to **255 characters** and rejects
  256 (lead re-ran the boundary test with the synthetic fixture host).
- The Floodgate marker is appended after a NUL byte, so the Java hostname plus the marker must fit
  in 255 characters. A long Bedrock name or a long configured `domains` value can push the handshake
  over the limit. Keep the Infrarust hostname (the Geyser `remote.address`) short.
- This is solved in config. No core change is needed or proposed.
- The measurement covers the parser only. Whether Geyser's real marker length stays within the cap is **NOT RUN**.

## Validation and status matrix

Use `PASS`, `FAIL`, `NOT RUN`, or `N/A`. Attach evidence for each `PASS`.

| # | Check | Evidence | Status |
|---|---|---|---|
| 1 | Core codec/identity tests (36) | Lead re-ran | PASS |
| 2 | Synthetic Python AES-GCM fixture decodes in Rust (not a live capture) | Lead re-ran | PASS |
| 3 | beta.4 `SHandshake` parser: 255-char host parses, 256-char host fails | Lead re-ran | PASS |
| 4 | WASM `clippy -D warnings`, `wasm32-wasip2` | Lead re-ran | PASS |
| 5 | Release `wasm32-wasip2` component build | Lead re-ran | PASS |
| 6 | Handshake length cap measured with the Geyser marker length (not only the synthetic fixture) | | NOT RUN |
| 7 | Infrarust `2.0.0-beta.4` loads the example `infrarust.toml` and server file | | NOT RUN |
| 8 | Adapter loads with `receive_proxy_protocol = false` and refuses when `true` | | NOT RUN |
| 9 | Key mount is read-only in the guest | | NOT RUN |
| 10 | Key file absent from git, logs and artifacts | | NOT RUN |
| 11 | Java port reachable only from Geyser's address (firewall) | | NOT RUN |
| 12 | Real Geyser Standalone + Floodgate handshake accepted | | NOT RUN |
| 13 | Real Bedrock login reaches the Fabric backend | | NOT RUN |
| 14 | Backend receives no Floodgate marker (`domain_rewrite`) | | NOT RUN |
| 15 | Replayed IV rejected within TTL | | NOT RUN |
| 16 | Java account with online auth still enforced | | NOT RUN |
| 17 | UUID and Java name match `UUID(0, xuid)` and the prefix rule | | NOT RUN |
| 18 | Logs contain no key or marker bytes | | NOT RUN |
| 19 | Codec accepts a real Floodgate/Geyser capture | | NOT RUN |

## Open questions

1. Geyser's config key names and the release they belong to are not checked (see the example).
2. Whether Geyser's `remote.address` produces exactly the Infrarust `domains` value in the handshake.
3. Whether the adapter's read of `raw_host` runs before `domain_rewrite` (not run).
4. Behavior of a restart with respect to the replay cache, under real traffic.
5. Whether a real Geyser marker stays within the 255-character hostname limit measured by the parser test.
