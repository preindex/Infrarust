---
title: Discord Community Gate
description: "Link Minecraft accounts to Discord and restrict client_only backends to members of configured Discord communities."
---

# Discord Community Gate

The Discord plugin runs inside the proxy. It starts a Discord bot, links authenticated Minecraft accounts to Discord users, and checks community membership before the proxy connects a player to a protected backend. Backend Minecraft servers do not need a plugin.

The plugin is compiled into the standard binary through the `plugin-discord` feature. Set a bot token in the environment or in the plugin's private config file:

```toml
# plugins/discord/config.toml
[bot]
token = ""
# application_id = 123456789012345678 # Optional; otherwise read from Discord

[links]
code_ttl_seconds = 600
max_failed_attempts = 5
failure_window_seconds = 600
max_pending_codes = 10000

[gate]
bypass_uuids = ["00000000-0000-0000-0000-000000000000"]

[membership]
positive_cache_seconds = 300
negative_cache_seconds = 60

# Extra gate: each configured guild requires at least one listed role.
# Guilds omitted here keep the member-only behavior above.
[role_gate.guild_roles]
# "123456789012345678" = ["234567890123456789"]

# Optional Discord role grants for the configured Minecraft permission nodes.
[role_permissions.guild_roles]
# [role_permissions.guild_roles."123456789012345678"]
# "234567890123456789" = ["example.chat", "example.warp"]

[moderation]
# Imports allowlisted Discord guild bans for linked UUIDs into the proxy.
# The bot needs Ban Members permission in those guilds.
# Imported bans use source "plugin:discord-sync" and are removed only after
# an unban is confirmed across the configured guilds.
sync_bans = false
guild_ids = []
# These roles are required for all staff slash commands.
staff_role_ids = []

[voice]
enabled = false
scan_interval_seconds = 5
# [[voice.routes]]
# server = "survival"
# guild_id = 123456789012345678
# voice_channel_ids = [234567890123456789, 345678901234567890]
# radius_blocks = 32.0

[nicknames]
# Requires Manage Nicknames. Sync only affects linked users in listed guilds.
enabled = false
guild_ids = []

[console]
# Dispatches only exact complete command lines listed below.
enabled = false
allowed_channel_ids = []
allowed_user_ids = []
allowed_role_ids = []
# Map an exact complete command line to the command's required permission node.
# [console.commands]
# "server list" = "infrarust.server.list"

[chat]
messages_per_minute = 8

[[chat.routes]]
server = "survival"
channel_id = 123456789012345678

[messages]
minecraft_to_discord = "**[{server}] {player}:** {message}"
discord_to_minecraft = "[Discord] {player}: {message}"
player_joined = "{player} joined {server}"
player_left = "{player} left {server}"
server_switched = "{player} moved from {reason} to {server}"
player_kicked = "{player} was kicked from {server}: {reason}"
```

Replace the example UUID with trusted operator Mojang UUIDs. These UUIDs bypass linking and membership checks on every Discord-gated server, including when no bot token is configured. Other players are refused until a token is configured. Keep this list in the private plugin config; it is never accepted from Docker labels.

When the admin web UI is enabled, the Discord plugin page provides a dashboard with a linked-account count, a table of Minecraft UUID and Discord ID links, and a confirmed unlink action. It also has a host-rendered settings form for link-code lifetime and limits, UUID bypasses, and membership cache durations. Saving these settings updates only those known fields in `plugins/discord/config.toml` and requires restarting Infrarust. The page never reads, displays, or changes the bot token; manage it with the environment, mounted secret file, or private config as described below.

For Docker deployments, mount the token as a Docker secret and set `INFRARUST_BOT_TOKEN_FILE` to its in-container path. The plugin reads and trims that file at startup. `INFRARUST_BOT_TOKEN` and `INFRARUST_DISCORD_BOT_TOKEN` are also accepted as direct environment-variable aliases, followed by `INFRARUST_DISCORD_BOT_TOKEN_FILE`; the private plugin config remains a local fallback. The bot must be installed in each community configured on a protected server, with permission to read guild members. The MVP checks membership with targeted Discord API requests and does not require the privileged Guild Members intent.

## Gate a server

Use the generic admission policy on the proxy-discovered server, or equivalent server config fields:

```yaml
labels:
  infrarust.proxy_mode: client_only
  infrarust.admissionGuards: discord
  infrarust.admissionPolicy.discord: '{"communities":["123456789012345678","234567890123456789","345678901234567890"]}'
```

The `discord` admission policy owns the `communities` list. A linked player is allowed when they are a member of **any one** listed community. The bot must be in every listed guild. The generic policy is validated by the plugin; malformed values fail closed.

The old labels `infrarust.whitelistThroughDiscord`, `infrarust.discordCommunity`, and `infrarust.discordCommunities` remain deprecated aliases. The whitelist label requires `client_only`; invalid community IDs invalidate the legacy list. The bot token must never be placed in Docker labels.

## Route chat and lifecycle messages

Add one `[[chat.routes]]` entry for each Minecraft server that should bridge to Discord. Each entry maps a server name to one Discord text channel ID; channel IDs must be unique. The bot needs permission to view and send messages in those channels. Discord-to-Minecraft messages go only to players currently connected to the mapped server. Minecraft chat, player join/leave, server switches, and backend kick notices go to that server's mapped channel. These notices are limited to routed servers.

The bot requests Discord's privileged Message Content intent. Enable **Message Content Intent** for the application in the Discord developer portal. Bot messages and webhook messages are ignored, Discord pings are disabled on outbound messages, and `@` is neutralized before Discord text is displayed in Minecraft. Chat is rate-limited per player/account using `messages_per_minute` over a rolling minute.

All `[messages]` values are configurable templates, so operators can translate them. For chat and lifecycle notices, the supported placeholders are `{player}`, `{server}`, `{message}`, and `{reason}`. For server switches, `{reason}` contains the previous server name; for kick notices it contains the backend reason or kick cause. Messages are truncated to fit Discord's message limit.

### Localize bot replies

Every reply the bot sends, including `/link`, `/unlink`, `/status`, `/players`, `/whois`, `/ban`, `/unban`, `/console`, and their denial, failure, and usage responses, comes from a `[messages]` key. Command names and option names are not localized. Discord's own slash-command descriptions are not affected.

Each key below uses the English default shown in the default config. Placeholders are filled in a single pass, so a substituted value is never expanded again. Placeholders not listed for a key are left as written. Keys missing from an existing `config.toml` fall back to these defaults, so older files keep working.

| Key | Placeholders |
| --- | --- |
| `command_server_only`, `command_staff_only`, `command_unknown` | none |
| `link_usage`, `link_rate_limited`, `link_success`, `link_invalid_code`, `link_discord_taken`, `link_minecraft_taken`, `link_failed` | none |
| `unlink_success`, `unlink_not_linked`, `unlink_failed` | none |
| `status_online` | `{count}` online player count |
| `players_none` | none |
| `players_list` | `{count}` online player count, `{players}` comma-separated names |
| `players_list_truncated` | `{count}`, `{players}`, and `{more}`, the number of players left out of the list |
| `whois_invalid_uuid` | none |
| `whois_offline` | `{uuid}` |
| `whois_result` | `{username}`, `{uuid}`, `{discord}` (a Discord ID or `whois_not_linked`) |
| `whois_not_linked` | none |
| `ban_invalid_uuid`, `ban_invalid_duration` | none |
| `ban_existing` | `{uuid}`, `{source}` the existing ban's source |
| `ban_lookup_failed` | `{error}` |
| `ban_success` | `{uuid}`, `{expires}` Unix seconds, or `ban_expires_never` |
| `ban_expires_never` | none |
| `ban_failed` | `{uuid}`, `{error}` |
| `unban_invalid_uuid`, `unban_success`, `unban_not_found` | `{uuid}` for `unban_success` and `unban_not_found` |
| `unban_failed` | `{uuid}`, `{error}` |
| `console_disabled`, `console_channel_required`, `console_channel_denied`, `console_user_denied`, `console_line_required`, `console_line_invalid`, `console_not_allowlisted`, `console_no_output`, `console_denied`, `console_unknown`, `console_unsupported` | none |

Values from Discord users and Minecraft players, including player names, error text, and ban sources, are neutralized before they are inserted. `@` is broken with a zero-width space, and Markdown characters (backslash, asterisk, underscore, tilde, pipe, backtick, and the angle brackets used for mentions) are backslash-escaped. Console output uses the same rules. Replies are cut to Discord's 2000-character message limit. Player lists show up to 40 names.

Two strings are not localized through `[messages]`. The default reason stored with a command ban, "No reason provided," and the reason stored with a Discord-synced ban, "Discord guild ban," are part of the proxy ban record and may be shown to banned players.

## Backend alerts and proximity voice

The optional Paper companion reports death, advancement, and (when enabled) player-position events over the private `infrarust:discord` plugin channel. Install `integrations/infrarust-discord-companion` on each Paper backend, configure a unique 32-byte shared secret on that backend and in `plugins/discord/backend-events.toml`, and enable only the event types needed. The proxy verifies the message HMAC, backend identity, carrier UUID, payload size, freshness, and rate before accepting it. Death and advancement alerts are routed to their configured channels. See [the companion setup guide](../../guide/discord-companion.md).

Discord proximity voice is opt-in with `[voice].enabled = true`. Add a `[[voice.routes]]` mapping for a backend, guild, a pool of dedicated voice channels, and a radius in blocks. Enable `positions_enabled` for that backend in `backend-events.toml` and `events.position` in the Paper companion. The bot needs the Guild Voice States intent and Move Members permission. Only linked Discord members already in one of the configured pool channels are considered; players must have a fresh authenticated position on the same backend and world. Nearby connected groups are assigned to the pool in stable order; groups beyond the available channel count stay where they are. This moves users between Discord voice channels; it does not add Minecraft in-game voice chat and needs no client mod. The configured channels should be dedicated to this feature; moving out of the pool removes the member from future grouping.

## Link an account

When an authenticated player first reaches a protected server without a link, the proxy places them in its local limbo world. A large six-digit code appears as the title, with a short `/link` prompt and expiry countdown. In Discord, use `/link` in a shared server or DM and enter the code in its required `code` option; the bot replies privately.

After the link is saved, the waiting Minecraft session resumes automatically. The proxy checks membership in at least one configured guild again before connecting to the protected backend. A missing limbo handler, expired code, unavailable authorizer, or failed membership check never grants backend access. No separate `limbo_handlers` label is required for Discord linking. Use `/unlink` from the Discord account to remove the link.

Links are stored in `plugins/discord/links.json`; pending one-time codes and failed-attempt counters are memory-only and expire. Links are one-to-one: a Minecraft account and a Discord account can each have only one counterpart. Unlinking does not kick an already-connected player; the gate is checked on the next connection or server switch.

Discord API failures deny a protected connection unless the membership result is available from the plugin's short-lived cache. If the plugin is missing, disabled, or lacks a token, protected servers fail closed while ungated servers continue to work.

## Console log forwarding

Proxy log lines can be sent to one Discord channel. This is off by default and needs two explicit operator steps, both required:

1. Grant the native capability in the proxy configuration. The capability is not part of the native defaults and is refused for WASM plugins:

   ```toml
   [plugins.discord]
   permissions = ["log-read"]
   ```

2. Enable forwarding in the plugin's `config.toml`:

   ```toml
   [log_forward]
   enabled = true
   channel_id = 123456789012345678
   min_level = "warn"
   targets = ["infrarust_core::session"]
   max_messages_per_minute = 6
   ```

Forwarding starts only when the bot token is configured and both steps are present. A missing grant or invalid setting is logged at startup, and the plugin keeps running without forwarding.

**Sensitive data.** Log lines are not reviewed before they are posted. They can contain player names and UUIDs, server addresses, operational detail, and anything another component writes to the log. The proxy removes IPv4 addresses and values of keys such as `password`, `token`, `secret`, and `authorization` on a best-effort basis. It does not remove IPv6 addresses, or secrets with no recognizable key. Use a private staff channel, keep the target list narrow, and do not forward at `debug` or `trace` unless you have checked what those lines contain.

**Which lines are seen.** The forwarder sees only events that pass the proxy's global log filter (`RUST_LOG` or `--log-level`). Lines that the proxy never emits cannot be forwarded, whatever `min_level` says.

**Limits and loop protection.**

- `targets` is an allowlist of module paths. Each path matches itself and its children, and at most 8 entries are accepted. An empty list disables forwarding and is logged at startup.
- Records from this plugin and from its Discord HTTP and gateway dependencies (`twilight`, `hyper`, `reqwest`, and similar) are never forwarded. A failed Discord send therefore cannot feed back into the forwarder.
- Lines are batched for a few seconds and sent as one fenced code block. The plugin sends at most `max_messages_per_minute` messages. Lines past that limit are counted and summarized in a later message, not sent.
- Backticks are replaced so that a line cannot close the code block, and `@` is followed by a zero-width space. Mentions are also disabled on the send.
- Messages are kept under Discord's 2,000-character limit. A line that would overflow the batch is counted as not forwarded.
- Records that the queue cannot hold are dropped rather than slowing the proxy. The summary message reports how many were dropped.

## Current scope

The plugin provides account linking, join/switch membership checks, routed chat bridging and lifecycle notices, role gates, role-to-permission mappings, moderation commands, and optional backend death/advancement events through the Paper companion. Membership and role lookups share the configured cache TTL, so removed Discord roles may remain effective until the positive cache expires. A configured role gate adds a role requirement for that guild while retaining the linked-account membership requirement. Permission grants use explicit `guild_roles` role-to-node mappings; malformed IDs and permission nodes are ignored and logged. The provider only grants configured nodes and never derives nodes from Discord role names.

## Role permissions and moderation

To require a Discord role for a gated community, add the guild ID and one or more role IDs under `[role_gate.guild_roles]`. A linked player must be a member of at least one guild listed on the protected server. A guild with a role requirement counts as an allowed community only when the member holds at least one listed role; a guild without a role requirement retains the existing member-only behavior.

To map roles into Minecraft permissions, configure exact role ID to permission-node lists under `[role_permissions.guild_roles."GUILD_ID"]`. These mappings are supplied through the existing permission provider API. Keep the permission provider selection compatible with the Discord plugin in the proxy configuration. Only explicit node grants are produced; role names, wildcard nodes, and the proxy admin permission are rejected.

`/status` and `/players` are available in Discord guilds. `/whois`, `/ban`, and `/unban` require a member role listed in `moderation.staff_role_ids`. `/whois` accepts a Minecraft UUID and reports only online players. Moderation commands accept UUID targets; they do not resolve or guess IP addresses. `/ban` accepts an optional duration in seconds, with omission meaning permanent. Existing non-Discord bans are preserved by `/ban` and by imported Discord ban events.

Ban synchronization is off unless `moderation.sync_bans = true` and one or more `moderation.guild_ids` are configured. Synchronization is intentionally one-way: Discord guild ban events import a ban only when the account has an explicit link, targeting that UUID with source `plugin:discord-sync`. A Discord unban removes only a proxy ban with that source, and only after the linked account is confirmed unbanned in every configured guild; lookup failures preserve the proxy ban. Proxy bans are not mirrored to Discord. Discord bans have no expiry metadata, so an imported ban remains until a Discord unban is received and confirmed.

Each gateway `READY` event, including startup and every reconnect, reconciles ban state for linked accounts. The plugin re-checks each linked account against every configured guild's ban list, one account at a time, so heartbeats are never blocked. A confirmed ban in any guild imports or keeps the `plugin:discord-sync` proxy ban. A confirmed "not banned" in every guild removes only a `plugin:discord-sync` ban. Any lookup error in any guild preserves the current proxy ban, and bans from other sources are never changed. This recovers ban events missed while the gateway was down, but it is still one-way and never removes a proxy ban that a Discord lookup could not confirm.

Discord-role group sync is a separate, opt-in Paper companion feature configured per backend under `[backends.group_sync]` in `backend-events.toml`. It maps linked roles to an explicit `allowed_groups` list. See `integrations/infrarust-discord-companion/README.md` for LuckPerms setup, the signed wire format, and removal semantics.

Linked nickname updates are opt-in with `nicknames.enabled = true` and only touch guilds listed in `nicknames.guild_ids`. The bot needs Discord's Manage Nicknames permission. Nicknames are updated after `/link` when the Minecraft player is still online, and again at later logins, if the bot can edit that member; failures are logged and do not affect account linking.

`/console` is disabled by default. To enable it, set `console.enabled = true`, configure allowed channel IDs, and configure allowed user IDs and/or role IDs. A caller must be in an allowed channel and match an allowed user ID or hold an allowed role. Each entry in `[console.commands]` is an exact complete command line, including any fixed arguments, mapped to that registered command's required permission node. The submitted line must exactly match one entry and is limited to 100 characters; the bot does not accept appended arguments or arbitrary console input. Remote command dispatch requires the command's own permission check and denies commands without an explicit permission node. Replies are ephemeral, mentions are neutralized, and output is capped at 1,800 UTF-16 code units. The bot acknowledges first and returns the command result as a follow-up. This is a scoped dispatcher for registered proxy commands, not a live console log stream or backend console.
Membership is cached for the configured TTL, so a removed member may retain access until the positive cache expires. Automatic kicks after a link or membership is revoked, cross-proxy shared storage, reverse in-game nickname display synchronization, and backend permission-group synchronization (such as LuckPerms) are not included. Console log forwarding exists but is off by default, see [Console log forwarding](#console-log-forwarding). The companion is Paper-specific; other backends need their own adapter implementing the documented authenticated wire format.

Remote command dispatch is the one small Infrarust API/core addition in this work: plugins now have a capability-gated `CommandManager::dispatch` entry point and a constrained remote source. The `command-dispatch` capability is native-trusted only and unavailable to WASM configuration. Chat, proxy-visible lifecycle events, permissions, and bans otherwise use existing plugin APIs.
