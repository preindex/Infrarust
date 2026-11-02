//! Private plugin configuration.
//!
//! The bot token lives here or in the `INFRARUST_BOT_TOKEN`
//! environment variable, never in Docker labels or server configuration.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Environment variable used by the standard Portainer deployment.
pub const TOKEN_ENV: &str = "INFRARUST_BOT_TOKEN";
/// Optional path to a Docker secret file containing the bot token.
pub const TOKEN_FILE_ENV: &str = "INFRARUST_BOT_TOKEN_FILE";
/// Backward-compatible, plugin-specific environment variable alias.
const TOKEN_ENV_ALIAS: &str = "INFRARUST_DISCORD_BOT_TOKEN";
/// Optional file-path alias for deployments that use the older variable name.
const TOKEN_FILE_ENV_ALIAS: &str = "INFRARUST_DISCORD_BOT_TOKEN_FILE";

const DEFAULT_CONFIG: &str = r#"# Infrarust Discord plugin configuration.
#
# The bot token is secret. It can also be supplied through
# INFRARUST_BOT_TOKEN or INFRARUST_BOT_TOKEN_FILE, which take precedence.

[bot]
token = ""
# Optional. When unset, the application ID is read from Discord at startup.
# application_id = 0

[links]
code_ttl_seconds = 600
max_failed_attempts = 5
failure_window_seconds = 600
max_pending_codes = 10000

[gate]
# Mojang UUIDs that may bypass Discord linking and membership checks.
bypass_uuids = []

[membership]
positive_cache_seconds = 300
negative_cache_seconds = 60

# Role requirements are an additional per-guild check after membership. Guilds
# omitted here keep the existing member-only gate.
[role_gate.guild_roles]
# "123456789012345678" = ["234567890123456789"]

# Explicit Discord role to Minecraft permission-node mapping.
[role_permissions.guild_roles]
# [role_permissions.guild_roles."123456789012345678"]
# "234567890123456789" = ["example.chat", "example.warp"]

[moderation]
# Imports Discord guild bans into the proxy ban service. Disabled by default.
sync_bans = false
guild_ids = []
staff_role_ids = []

[nicknames]
# Requires Manage Nicknames; only updates linked members in listed guilds.
enabled = false
guild_ids = []

[console]
# Allows only exact command lines listed under [console.commands].
enabled = false
allowed_channel_ids = []
allowed_user_ids = []
allowed_role_ids = []
# [console.commands]
# "server list" = "infrarust.server.list"
[log_forward]
# Forwards proxy log lines to one Discord channel. Off by default.
# Log lines can contain player names, UUIDs, IP addresses and operational
# detail. Use a private staff channel, and grant the capability in the proxy
# config with permissions = ["log-read"] under [plugins.discord].
enabled = false
channel_id = 0
# Minimum severity: "error", "warn", "info", "debug" or "trace".
min_level = "warn"
# Module paths to forward. A path matches itself and its children, so
# "infrarust_core" also matches "infrarust_core::session".
targets = []
# Discord messages sent per minute. Lines past this limit are counted and summarized.
max_messages_per_minute = 6

[chat]
# Enable Discord-to-Minecraft and Minecraft-to-Discord chat for mapped servers.
messages_per_minute = 8
# Add one route per Minecraft server. A channel can belong to only one server.
# [[chat.routes]]
# server = "survival"
# channel_id = 123456789012345678

[messages]
# Translate these templates to localize the plugin. Each key lists its own
# placeholders; unknown placeholders are left as written. Keys missing from an
# older file use the English defaults.
# Chat and lifecycle: {player}, {server}, {message}, {reason}.
minecraft_to_discord = "**[{server}] {player}:** {message}"
discord_to_minecraft = "[Discord] {player}: {message}"
player_joined = "{player} joined {server}"
player_left = "{player} left {server}"
server_switched = "{player} moved from {reason} to {server}"
player_kicked = "{player} was kicked from {server}: {reason}"
# Command replies (no placeholders unless listed).
command_server_only = "Use this command in a Discord server."
command_staff_only = "This command is restricted to configured Discord staff roles."
command_unknown = "Unknown command."
link_usage = "Usage: /link code:<the code shown when you tried to join the server>"
link_rate_limited = "Too many failed attempts. Try again in a few minutes."
link_success = "Your Discord account is now linked. If you are waiting in Minecraft, you will join automatically once community membership is verified."
link_invalid_code = "That code is invalid or has expired. Reconnect to the server to get a new one."
link_discord_taken = "This Discord account is already linked to a different Minecraft account. Run /unlink first if you want to change it."
link_minecraft_taken = "That Minecraft account is already linked to a different Discord account. Its owner must run /unlink from that account first."
link_failed = "The link could not be saved. Try again later."
unlink_success = "Your Minecraft link has been removed."
unlink_not_linked = "This Discord account has no Minecraft link."
unlink_failed = "The link could not be removed. Try again later."
# {count}: online player count.
status_online = "Proxy Discord integration is online. {count} Minecraft player(s) are connected."
players_none = "No players are currently online."
# {count}: online player count; {players}: comma-separated names.
players_list = "Online ({count}): {players}"
# {count}, {players} as above; {more}: players left out of the list.
players_list_truncated = "Online ({count}): {players}, and {more} more."
whois_invalid_uuid = "Provide a valid Minecraft UUID."
# {uuid}: the requested UUID.
whois_offline = "{uuid} is not currently online."
# {username}, {uuid}, and {discord} (a Discord ID or whois_not_linked).
whois_result = "{username} ({uuid}) is online; Discord link: {discord}."
whois_not_linked = "not linked"
ban_invalid_uuid = "Provide a valid Minecraft UUID. No IP address is inferred."
ban_invalid_duration = "Duration must be between 1 and 31,536,000 seconds."
# {uuid}, {source}: the existing ban's source.
ban_existing = "{uuid} already has a ban from {source}."
# {error}: the lookup error.
ban_lookup_failed = "Could not inspect existing bans: {error}"
# {uuid}, {expires}: Unix seconds, or ban_expires_never.
ban_success = "Banned {uuid}. Expires: {expires}."
ban_expires_never = "never"
# {uuid}, {error}.
ban_failed = "Could not ban {uuid}: {error}"
unban_invalid_uuid = "Provide a valid Minecraft UUID."
unban_success = "Unbanned {uuid}."
unban_not_found = "No ban was found for {uuid}."
# {uuid}, {error}.
unban_failed = "Could not unban {uuid}: {error}"
console_disabled = "Discord command dispatch is disabled."
console_channel_required = "This command is only available in configured Discord channels."
console_channel_denied = "This channel is not allowed to dispatch proxy commands."
console_user_denied = "You are not allowed to dispatch proxy commands."
console_line_required = "Select an exact command line from the configured allowlist."
console_line_invalid = "The command line is invalid."
console_not_allowlisted = "That exact command line is not allowlisted."
console_no_output = "Command completed with no output."
console_denied = "The proxy denied this command."
console_unknown = "The configured command is not registered."
console_unsupported = "The proxy returned an unsupported dispatch result."

[voice]
# Opt-in proximity grouping requires the Paper companion position feed.
enabled = false
scan_interval_seconds = 5
# [[voice.routes]]
# server = "survival"
# guild_id = 123456789012345678
# voice_channel_ids = [234567890123456789, 345678901234567890]
# radius_blocks = 32.0
"#;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config file I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("config file is invalid: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("no Discord bot token: set INFRARUST_BOT_TOKEN or [bot] token")]
    MissingToken,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct DiscordConfig {
    pub bot: BotConfig,
    pub links: LinkConfig,
    pub gate: GateConfig,
    pub membership: MembershipConfig,
    pub role_gate: RoleGateConfig,
    pub role_permissions: RolePermissionConfig,
    pub moderation: ModerationConfig,
    pub nicknames: NicknameConfig,
    pub console: ConsoleConfig,
    pub log_forward: LogForwardConfig,
    pub chat: ChatConfig,
    pub messages: MessageTemplates,
    pub voice: VoiceConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct LogForwardConfig {
    pub enabled: bool,
    pub channel_id: u64,
    pub min_level: String,
    pub targets: Vec<String>,
    pub max_messages_per_minute: usize,
}

impl Default for LogForwardConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            channel_id: 0,
            min_level: "warn".to_owned(),
            targets: Vec::new(),
            max_messages_per_minute: 6,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct VoiceConfig {
    pub enabled: bool,
    pub scan_interval_seconds: u64,
    pub routes: Vec<VoiceRoute>,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            scan_interval_seconds: 5,
            routes: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct VoiceRoute {
    pub server: String,
    pub guild_id: u64,
    pub voice_channel_ids: Vec<u64>,
    pub radius_blocks: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ChatConfig {
    pub messages_per_minute: usize,
    pub routes: Vec<ChatRoute>,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            messages_per_minute: 8,
            routes: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChatRoute {
    pub server: String,
    pub channel_id: u64,
}

/// Operator-editable text. Every field has a default, so a `[messages]` table
/// from an older `config.toml` that lacks newer keys still loads.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct MessageTemplates {
    pub minecraft_to_discord: String,
    pub discord_to_minecraft: String,
    pub player_joined: String,
    pub player_left: String,
    pub server_switched: String,
    pub player_kicked: String,
    pub command_server_only: String,
    pub command_staff_only: String,
    pub command_unknown: String,
    pub link_usage: String,
    pub link_rate_limited: String,
    pub link_success: String,
    pub link_invalid_code: String,
    pub link_discord_taken: String,
    pub link_minecraft_taken: String,
    pub link_failed: String,
    pub unlink_success: String,
    pub unlink_not_linked: String,
    pub unlink_failed: String,
    pub status_online: String,
    pub players_none: String,
    pub players_list: String,
    pub players_list_truncated: String,
    pub whois_invalid_uuid: String,
    pub whois_offline: String,
    pub whois_result: String,
    pub whois_not_linked: String,
    pub ban_invalid_uuid: String,
    pub ban_invalid_duration: String,
    pub ban_existing: String,
    pub ban_lookup_failed: String,
    pub ban_success: String,
    pub ban_expires_never: String,
    pub ban_failed: String,
    pub unban_invalid_uuid: String,
    pub unban_success: String,
    pub unban_not_found: String,
    pub unban_failed: String,
    pub console_disabled: String,
    pub console_channel_required: String,
    pub console_channel_denied: String,
    pub console_user_denied: String,
    pub console_line_required: String,
    pub console_line_invalid: String,
    pub console_not_allowlisted: String,
    pub console_no_output: String,
    pub console_denied: String,
    pub console_unknown: String,
    pub console_unsupported: String,
}

impl Default for MessageTemplates {
    fn default() -> Self {
        Self {
            minecraft_to_discord: "**[{server}] {player}:** {message}".into(),
            discord_to_minecraft: "[Discord] {player}: {message}".into(),
            player_joined: "{player} joined {server}".into(),
            player_left: "{player} left {server}".into(),
            server_switched: "{player} moved from {reason} to {server}".into(),
            player_kicked: "{player} was kicked from {server}: {reason}".into(),
            command_server_only: "Use this command in a Discord server.".into(),
            command_staff_only: "This command is restricted to configured Discord staff roles."
                .into(),
            command_unknown: "Unknown command.".into(),
            link_usage: "Usage: /link code:<the code shown when you tried to join the server>"
                .into(),
            link_rate_limited: "Too many failed attempts. Try again in a few minutes.".into(),
            link_success: "Your Discord account is now linked. If you are waiting in Minecraft, you will join automatically once community membership is verified.".into(),
            link_invalid_code: "That code is invalid or has expired. Reconnect to the server to get a new one.".into(),
            link_discord_taken: "This Discord account is already linked to a different Minecraft account. Run /unlink first if you want to change it.".into(),
            link_minecraft_taken: "That Minecraft account is already linked to a different Discord account. Its owner must run /unlink from that account first.".into(),
            link_failed: "The link could not be saved. Try again later.".into(),
            unlink_success: "Your Minecraft link has been removed.".into(),
            unlink_not_linked: "This Discord account has no Minecraft link.".into(),
            unlink_failed: "The link could not be removed. Try again later.".into(),
            status_online: "Proxy Discord integration is online. {count} Minecraft player(s) are connected.".into(),
            players_none: "No players are currently online.".into(),
            players_list: "Online ({count}): {players}".into(),
            players_list_truncated: "Online ({count}): {players}, and {more} more.".into(),
            whois_invalid_uuid: "Provide a valid Minecraft UUID.".into(),
            whois_offline: "{uuid} is not currently online.".into(),
            whois_result: "{username} ({uuid}) is online; Discord link: {discord}.".into(),
            whois_not_linked: "not linked".into(),
            ban_invalid_uuid: "Provide a valid Minecraft UUID. No IP address is inferred.".into(),
            ban_invalid_duration: "Duration must be between 1 and 31,536,000 seconds.".into(),
            ban_existing: "{uuid} already has a ban from {source}.".into(),
            ban_lookup_failed: "Could not inspect existing bans: {error}".into(),
            ban_success: "Banned {uuid}. Expires: {expires}.".into(),
            ban_expires_never: "never".into(),
            ban_failed: "Could not ban {uuid}: {error}".into(),
            unban_invalid_uuid: "Provide a valid Minecraft UUID.".into(),
            unban_success: "Unbanned {uuid}.".into(),
            unban_not_found: "No ban was found for {uuid}.".into(),
            unban_failed: "Could not unban {uuid}: {error}".into(),
            console_disabled: "Discord command dispatch is disabled.".into(),
            console_channel_required: "This command is only available in configured Discord channels.".into(),
            console_channel_denied: "This channel is not allowed to dispatch proxy commands.".into(),
            console_user_denied: "You are not allowed to dispatch proxy commands.".into(),
            console_line_required: "Select an exact command line from the configured allowlist.".into(),
            console_line_invalid: "The command line is invalid.".into(),
            console_not_allowlisted: "That exact command line is not allowlisted.".into(),
            console_no_output: "Command completed with no output.".into(),
            console_denied: "The proxy denied this command.".into(),
            console_unknown: "The configured command is not registered.".into(),
            console_unsupported: "The proxy returned an unsupported dispatch result.".into(),
        }
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct BotConfig {
    pub token: String,
    pub application_id: Option<u64>,
}

// Hand-written so the token never reaches a debug log.
impl std::fmt::Debug for BotConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotConfig")
            .field("token", &"<redacted>")
            .field("application_id", &self.application_id)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct LinkConfig {
    pub code_ttl_seconds: u64,
    pub max_failed_attempts: usize,
    pub failure_window_seconds: u64,
    pub max_pending_codes: usize,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            code_ttl_seconds: 600,
            max_failed_attempts: 5,
            failure_window_seconds: 600,
            max_pending_codes: 10_000,
        }
    }
}

impl LinkConfig {
    pub fn code_ttl(&self) -> Duration {
        Duration::from_secs(self.code_ttl_seconds.max(1))
    }

    pub fn failure_window(&self) -> Duration {
        Duration::from_secs(self.failure_window_seconds.max(1))
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct GateConfig {
    /// Authenticated Mojang UUIDs exempt from Discord linking and membership.
    pub bypass_uuids: Vec<uuid::Uuid>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct MembershipConfig {
    pub positive_cache_seconds: u64,
    pub negative_cache_seconds: u64,
}

impl Default for MembershipConfig {
    fn default() -> Self {
        Self {
            positive_cache_seconds: 300,
            negative_cache_seconds: 60,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct RoleGateConfig {
    pub guild_roles: std::collections::HashMap<String, Vec<String>>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct RolePermissionConfig {
    pub guild_roles:
        std::collections::HashMap<String, std::collections::HashMap<String, Vec<String>>>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ModerationConfig {
    pub sync_bans: bool,
    pub guild_ids: Vec<String>,
    pub staff_role_ids: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct NicknameConfig {
    pub enabled: bool,
    pub guild_ids: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ConsoleConfig {
    pub enabled: bool,
    pub allowed_channel_ids: Vec<String>,
    pub allowed_user_ids: Vec<String>,
    pub allowed_role_ids: Vec<String>,
    /// Exact complete command line to the permission node required by it.
    pub commands: std::collections::HashMap<String, String>,
}

impl MembershipConfig {
    pub fn positive_ttl(&self) -> Duration {
        Duration::from_secs(self.positive_cache_seconds)
    }

    pub fn negative_ttl(&self) -> Duration {
        Duration::from_secs(self.negative_cache_seconds)
    }
}

impl DiscordConfig {
    /// Loads `config.toml`, writing the commented default when it is missing.
    pub async fn load_or_create(path: &Path) -> Result<Self, ConfigError> {
        match tokio::fs::read_to_string(path).await {
            Ok(content) => Ok(toml::from_str(&content)?),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(path, DEFAULT_CONFIG).await?;
                Ok(toml::from_str(DEFAULT_CONFIG)?)
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Resolves the bot token from environment, mounted secret file, or config.
    pub fn resolve_token(&self) -> Result<String, ConfigError> {
        for name in [TOKEN_ENV, TOKEN_ENV_ALIAS] {
            if let Some(value) = std::env::var(name)
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
            {
                return Ok(value);
            }
        }

        for name in [TOKEN_FILE_ENV, TOKEN_FILE_ENV_ALIAS] {
            if let Some(path) = std::env::var(name)
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
            {
                let value = std::fs::read_to_string(path)?;
                let value = value.trim();
                return if value.is_empty() {
                    Err(ConfigError::MissingToken)
                } else {
                    Ok(value.to_owned())
                };
            }
        }

        let file = self.bot.token.trim();
        (!file.is_empty())
            .then(|| file.to_owned())
            .ok_or(ConfigError::MissingToken)
    }
}
