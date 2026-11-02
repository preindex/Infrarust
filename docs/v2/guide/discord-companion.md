---
title: Discord backend companion
description: Optional Paper companion for relaying signed Minecraft events and bounded position snapshots to an Infrarust proxy.
---

# Discord backend companion

The optional `InfrarustDiscordCompanion` Paper plugin relays death events, completed advancements, and (when enabled) player position snapshots to the trusted Infrarust proxy over a namespaced Minecraft plugin-message channel. The proxy-side Discord integration can use those events without parsing opaque Minecraft packets in proxy core. This companion does not call Discord APIs, send voice traffic, or run console commands.

## Install and configure

1. Build the artifact from `integrations/infrarust-discord-companion` with JDK 21 and Maven 3.9 or later (`mvn package`).
2. Copy `target/infrarust-discord-companion-0.1.0.jar` into the backend's `plugins/` directory and restart the Paper server.
3. Generate a unique random 32-byte secret for each backend, for example with `openssl rand -hex 32`. Configure the same secret in the proxy's trusted-backend configuration and in that backend's `plugins/InfrarustDiscordCompanion/config.yml`.
4. Set `enabled: true`. Death and advancement relays default on after opt-in; position relays default off. Keep the secret readable only by the backend and proxy operators.

The plugin stays disabled when `enabled` is false. If enabled with a missing or malformed secret, it disables itself and logs an error. The plugin registers an outgoing channel and event listeners only after successful configuration. Because Minecraft plugin messages ride an online player's backend connection, messages are not sent while there is no online player. The server's login behavior is unchanged whether this plugin is present or absent.

Example configuration:

```yaml
enabled: true
shared-secret: "<64 hex characters from a secure random generator>"
events:
  death: true
  advancement: true
  position: false
position-interval-ticks: 40
max-position-players: 128
```

`position-interval-ticks` is clamped to 20–1200 ticks. The plugin emits no more than one snapshot per selected player per interval and selects at most `max-position-players` players per cycle (clamped to 1–512). Position updates are opt-in because they reveal sensitive location data to the proxy.

## Wire protocol v1

Channel: `infrarust:discord` (registered as an outgoing Paper plugin channel).

All integers are big-endian. Strings are unsigned 16-bit byte length followed by UTF-8 bytes; decoding must reject malformed UTF-8, empty strings, or strings over the per-field byte limit. The proxy must reject an entire payload unless it is between the minimum for its event and 512 bytes, has no trailing bytes, uses the known magic/version/type, and has a valid HMAC.

| Field | Size | Meaning |
|---|---:|---|
| Magic | 4 bytes | ASCII `IRDC` |
| Version | 1 byte | `1` |
| Event type | 1 byte | `1` death, `2` advancement, `3` position |
| Event time | 8 bytes | Unix time in whole seconds, signed 64-bit representation |
| Player UUID | 16 bytes | Two signed 64-bit UUID halves |
| Event data | varies | Defined below |
| Authentication tag | 32 bytes | HMAC-SHA-256 over every preceding byte |

Event data:

- **Death (`1`)**: no event-specific fields.
- **Advancement (`2`)**: one length-prefixed namespaced advancement key, at most 128 UTF-8 bytes.
- **Position (`3`)**: one length-prefixed world key (at most 128 UTF-8 bytes), then X, Y, and Z as three IEEE-754 64-bit doubles.

The total encoded message is capped at 512 bytes. The proxy should also reject non-finite coordinates and timestamps outside its freshness window, and should rate-limit each backend/player. The companion limits position cadence; proxy-side limits remain necessary because a compromised backend can bypass them.

## Security and privacy

- The proxy must accept this channel only from an authenticated backend connection and match the configured secret for the actual backend identity. Do not trust a UUID or backend name supplied in the payload as proof of origin.
- Verify HMAC-SHA-256 in constant time, enforce a short timestamp freshness window, and rate-limit before processing. Timestamp validation narrows replay exposure; a receiver may additionally track duplicate event digests within the freshness window.
- Use distinct random secrets per backend and rotate them by updating proxy and backend configuration together. HMAC provides integrity and backend authentication, not confidentiality; the backend-to-proxy connection must be trusted and protected from untrusted intermediaries.
- The UUID, advancement/world keys, and coordinates are for the trusted proxy only. Do not forward raw snapshots or stable player identifiers to Discord. The future proxy integration should resolve Discord-facing display data and proximity calculations under its own privacy policy, and should avoid exposing coordinates in Discord output.
- Plugin messages require a connected player carrier. There is no durable queue or delivery acknowledgement; an event can be lost during a disconnect or when no player is online.

This companion deliberately uses Bukkit/Paper APIs only and targets Paper 1.21+ with a Java 21 runtime. Other forks may work if they preserve the Bukkit plugin messaging API, but are not covered by this integration's compatibility target.
