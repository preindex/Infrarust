# Infrarust Discord Companion

Optional Paper backend plugin that sends authenticated Minecraft event and position messages to an Infrarust proxy. It can also apply Discord-derived permission groups through LuckPerms when group sync is enabled. It does not connect to Discord, format Discord messages, or perform voice networking.

See [the companion integration guide](../../docs/v2/guide/discord-companion.md) for install steps, protocol details, and the trust model.

## Build

Requires JDK 21 and Maven 3.9 or later:

```sh
mvn package
```

The shaded artifact is not needed; install `target/infrarust-discord-companion-0.1.0.jar` on each opted-in Paper backend.

## Discord role group sync (optional, LuckPerms)

Group sync is off by default. It has two independent halves: the proxy decides which allowlisted groups each linked player should hold from their Discord roles, and this companion applies that set with LuckPerms. Enable it only on backends that need Discord-based permissions.

### Proxy configuration

Group sync is configured per backend in the proxy's `plugins/discord/backend-events.toml`, next to that backend's `shared_secret`:

```toml
[[backends]]
server = "survival"
shared_secret = "<the same 64 hex characters as this backend's config.yml>"

[backends.group_sync]
enabled = true
# Strict allowlist. Only these groups are ever granted or removed.
allowed_groups = ["vip", "builder"]
# How often each player on this backend is re-checked against Discord roles.
resync_interval_seconds = 600

[backends.group_sync.guild_roles."123456789012345678"]
"234567890123456789" = ["vip"]
"345678901234567890" = ["builder"]
```

Rules enforced by the proxy when it loads the file:

- `allowed_groups` has 1 to 16 entries. Each entry is lowercase, 1 to 64 characters from `a-z 0-9 _ - .`, and cannot be `default`.
- Every group in a role mapping must appear in `allowed_groups`. Anything else fails startup.
- `resync_interval_seconds` must be from 60 to 86400.
- Discord IDs must be nonzero integers. Group sync needs the bot token, because role lookups go through Discord.

Group sync sends only to Mojang-verified (online-mode) players. An offline-mode UUID derives from the username and cannot be trusted.

### Companion configuration

Set these in this backend's `plugins/InfrarustDiscordCompanion/config.yml`. `enabled` and `shared-secret` must already be set for the companion to run.

```yaml
enabled: true
shared-secret: "<the same 64 hex characters as the proxy's shared_secret>"

group-sync:
  enabled: true
  # Must equal this backend's `server` id in the proxy's backend-events.toml.
  server-id: "survival"
  # Must match the proxy's allowed_groups exactly. The companion enforces this list
  # independently, so a message naming any other group is rejected.
  allowed-groups:
    - vip
    - builder
```

If the companion cannot satisfy its settings, it logs a severe message and disables group sync only. The event relay keeps running. Install LuckPerms on any backend that enables group sync; without it the companion runs without group sync.

### LuckPerms setup

1. Create each group on the backend, using the same name as in `allowed-groups`, and give it the permissions it should carry:

   ```text
   /lp creategroup vip
   /lp group vip permission set essentials.fly true
   /lp creategroup builder
   /lp group builder permission set worldedit.* true
   ```

2. Sync grants each managed group directly to the player. It does not change group-to-group inheritance, so add `default` or other inheritance yourself if you want it.
3. The companion only removes groups it granted itself. Read the removal rules below before relying on manual grants of the same group.
4. If several backends share one LuckPerms database, give each backend a non-overlapping `allowed-groups` list. Two backends managing the same group would fight over it, and each one keeps its own ownership record.

### Removal semantics

The companion tracks which groups it granted in `plugins/InfrarustDiscordCompanion/owned-groups.yml`. The proxy sends the complete set of managed groups each player should hold, and the companion reconciles against that set:

| Situation | Result |
|---|---|
| Player has the Discord role, lacks the group | Group is added and recorded as owned |
| Player has the Discord role and already holds the group, not from this companion | Left alone. It is not recorded as owned, so it is never removed by sync |
| Player holds a group this companion owns and the role is gone | Group is removed and the ownership record is dropped |
| Player holds a group this companion owns and an admin removed it manually | Ownership record is dropped. Nothing is added back |
| Player is unlinked, or left the Discord guild | Empty set is sent. Companion-owned groups are removed. Distinct manual groups stay; a manual grant of the same group is indistinguishable and is removed |
| Discord lookup failed (rate limit, outage, bot removed from guild) | Nothing is sent. Existing groups stay unchanged |
| Group removed from `allowed-groups` | Stops being managed. Its existing grants are left in place and its ownership record is dropped at startup |

Consequences to plan for:

- If an admin grants a managed group before sync reaches the player, the companion does not claim it. If the Discord role is later lost, sync does not remove that grant.
- The reverse is a known limitation: if sync granted a group and an admin later grants the same group by hand, the companion still treats it as owned. It removes the group when the Discord role is lost. To keep a group permanently, remove it from `allowed-groups` or grant it under a different group name.
- The ownership file is per backend. Deleting it causes the companion to forget its grants, and those grants then stay until an admin removes them.

### Timing

- A sync is sent when a player reaches the backend (after the proxy completes the connection) and again every `resync_interval_seconds` for players already online.
- Role removals therefore take effect at the next resync, not instantly. Joining, or a resync, is the only trigger.
- The proxy sends at most 240 syncs per backend per minute, and the companion accepts at most 600 per minute. Syncs over the proxy cap are skipped and sent again on the next resync.
- A sync needs a player connected to the backend to carry it. With no carrier, the proxy drops the message and the next resync or join sends it again. A carrier still starting up holds the message until its backend connection reaches the play phase.

## Group sync wire protocol v1

Channel: `infrarust:discord_groups`. This is proxy-to-backend only. The backend registers it as an incoming channel. Integers are big-endian. The proxy sends one message per player per sync.

| Field | Size | Meaning |
|---|---:|---|
| Magic | 4 bytes | ASCII `IRDG` |
| Version | 1 byte | `1` |
| Message type | 1 byte | `1` sync |
| Server ID | 2 + N bytes | Length-prefixed UTF-8, 1 to 128 bytes. Must equal `group-sync.server-id` |
| Timestamp | 8 bytes | Signed 64-bit Unix time in **milliseconds** |
| Player UUID | 16 bytes | Mojang UUID, as two signed 64-bit halves |
| Group count | 1 byte | 0 to 16 |
| Groups | repeated 2 + N bytes | Length-prefixed UTF-8, 1 to 64 bytes each, all in `allowed-groups` |
| Authentication tag | 32 bytes | HMAC-SHA-256 over every preceding byte, keyed with the backend secret |

The whole message is at most 2048 bytes. The companion rejects a message for any of these reasons:

- Wrong length, magic, version, or type, or trailing bytes.
- HMAC mismatch. The check runs before any field is parsed, and it is constant-time.
- Server ID differs from `group-sync.server-id`. This binds the message to one backend, so a message for another backend's secret cannot be replayed here.
- Timestamp more than 30 seconds in the past or more than 5 seconds in the future.
- Timestamp not strictly newer than the last accepted timestamp for the same UUID. This rejects replays and reordered messages within the window.
- Invalid group name, a group outside `allowed-groups`, a duplicate group, or more than 16 groups.
- Rate limit reached (600 messages per minute, counted over every message on the channel).

The companion keeps the last accepted timestamp per UUID in memory, for up to 8192 players. After a restart it relies on the freshness window alone.

## Security notes

- Each backend needs its own random 32-byte secret, given to both the proxy and that backend. Rotate it by updating both sides together. Until both match, sync messages fail authentication and are dropped.
- The HMAC authenticates the proxy, and the UUID is bound to the proxy's Mojang-verified profile. The companion trusts the proxy's decision about which groups a player holds, so the proxy and its Discord role lookups are the authority.
- Group sync sends nothing to Discord. The proxy logs only server IDs, guild IDs, and error text for group sync, and never logs UUIDs or secrets.
- The companion writes UUIDs to `owned-groups.yml`, which stays on the backend and never leaves it. Keep the plugin data folder private.
