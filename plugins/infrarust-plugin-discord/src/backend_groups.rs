//! Opt-in sync of linked Discord roles into backend permission groups.
//!
//! For each configured backend, the proxy derives the allowlisted groups a
//! player should hold from their Discord roles and sends that complete set to
//! the backend's Paper companion, signed with the backend's secret. The
//! companion applies it and removes only groups it granted itself. Discord
//! output and logs never include player UUIDs or secrets.

use std::collections::{BTreeSet, HashMap};
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use infrarust_api::event::bus::{EventBus, EventBusExt};
use infrarust_api::event::{EventPriority, ListenerHandle};
use infrarust_api::events::ServerPostConnectEvent;
use infrarust_api::messaging::{ChannelId, MessagingError, ServerMessenger};
use infrarust_api::player::Player;
use infrarust_api::services::PlayerRegistry;
use infrarust_api::types::ServerId;
use serde::Deserialize;
use sha2::Sha256;
use tokio_util::sync::CancellationToken;
use twilight_model::id::Id;
use twilight_model::id::marker::GuildMarker;
use uuid::Uuid;

use crate::limits::SlidingWindow;
use crate::membership::{MembershipChecker, parse_guild, parse_user};
use crate::store::LinkBook;

/// Proxy-to-backend channel. It is separate from the backend-to-proxy
/// `infrarust:discord` event relay so the two message families never share a
/// parser.
const CHANNEL: &str = "infrarust:discord_groups";
const MAGIC: &[u8; 4] = b"IRDG";
const VERSION: u8 = 1;
const SYNC: u8 = 1;
const MAX_PAYLOAD: usize = 2048;
const MAX_GROUPS: usize = 16;
const MAX_GROUP_BYTES: usize = 64;
const MAX_SERVER_BYTES: usize = 128;
const DEFAULT_RESYNC_SECONDS: u64 = 600;
const MIN_RESYNC_SECONDS: u64 = 60;
const MAX_RESYNC_SECONDS: u64 = 86_400;
const RESYNC_TICK: Duration = Duration::from_secs(30);
/// Per-backend cap on syncs sent in one window. Syncs over the cap are
/// dropped, and the next resync pass sends them again.
const SYNCS_PER_WINDOW: usize = 240;
const SYNC_WINDOW: Duration = Duration::from_secs(60);
const MAX_TRACKED_PLAYERS: usize = 8192;
/// Matches the companion's freshness bounds.
const FRESHNESS_MS: i64 = 30_000;

type HmacSha256 = Hmac<Sha256>;

/// Per-backend group sync settings from `backend-events.toml`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct GroupSyncConfig {
    pub enabled: bool,
    /// Strict allowlist. Only these groups are ever granted or removed.
    pub allowed_groups: Vec<String>,
    pub resync_interval_seconds: u64,
    /// Guild ID to Discord role ID to permission groups.
    pub guild_roles: HashMap<String, HashMap<String, Vec<String>>>,
}

impl Default for GroupSyncConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allowed_groups: Vec::new(),
            resync_interval_seconds: DEFAULT_RESYNC_SECONDS,
            guild_roles: HashMap::new(),
        }
    }
}

/// Discord role ID to the permission groups it grants.
type RoleGroups = Vec<(u64, Vec<String>)>;

/// A validated, enabled group sync for one backend.
#[derive(Clone)]
pub struct GroupSyncRoute {
    server: String,
    secret: Vec<u8>,
    resync_interval: Duration,
    guild_roles: Vec<(Id<GuildMarker>, RoleGroups)>,
}

impl GroupSyncRoute {
    /// Validates an enabled backend's group sync. A disabled backend yields `None`.
    pub fn compile(
        server: &str,
        secret: &[u8],
        config: &GroupSyncConfig,
    ) -> Result<Option<Self>, String> {
        if !config.enabled {
            return Ok(None);
        }
        if server.is_empty() || server.len() > MAX_SERVER_BYTES {
            return Err(format!(
                "group sync for `{server}` has an invalid server id"
            ));
        }
        let allowed = parse_allowed_groups(server, &config.allowed_groups)?;
        if !(MIN_RESYNC_SECONDS..=MAX_RESYNC_SECONDS).contains(&config.resync_interval_seconds) {
            return Err(format!(
                "group sync for `{server}` needs resync_interval_seconds from {MIN_RESYNC_SECONDS} to {MAX_RESYNC_SECONDS}"
            ));
        }
        if config.guild_roles.is_empty() {
            return Err(format!(
                "group sync for `{server}` is enabled but has no guild_roles"
            ));
        }

        let mut guilds = Vec::with_capacity(config.guild_roles.len());
        for (guild_raw, role_map) in &config.guild_roles {
            let guild = parse_guild(guild_raw)
                .ok_or_else(|| format!("group sync for `{server}` has an invalid guild ID"))?;
            let mut roles = Vec::with_capacity(role_map.len());
            for (role_raw, groups) in role_map {
                let role = role_raw
                    .trim()
                    .parse::<NonZeroU64>()
                    .map_err(|_| format!("group sync for `{server}` has an invalid role ID"))?
                    .get();
                if groups.is_empty() {
                    return Err(format!(
                        "group sync for `{server}` maps a role to no groups"
                    ));
                }
                for group in groups {
                    if !allowed.contains(group) {
                        return Err(format!(
                            "group sync for `{server}` maps a role to `{group}`, which is not in allowed_groups"
                        ));
                    }
                }
                roles.push((role, groups.clone()));
            }
            guilds.push((guild, roles));
        }

        Ok(Some(Self {
            server: server.to_owned(),
            secret: secret.to_vec(),
            resync_interval: Duration::from_secs(config.resync_interval_seconds),
            guild_roles: guilds,
        }))
    }
}

/// Owns group sync state for every enabled backend.
pub struct GroupSync {
    routes: HashMap<String, GroupSyncRoute>,
    links: Arc<LinkBook>,
    membership: Arc<MembershipChecker>,
    messenger: Arc<dyn ServerMessenger>,
    channel: ChannelId,
    /// Last timestamp sent per player. Each new sync is strictly newer, so the
    /// companion can reject replays and reordered messages.
    last_issued: Mutex<HashMap<Uuid, i64>>,
    limiter: SlidingWindow,
}

impl GroupSync {
    /// Returns `None` when no backend enables group sync. Without the bot token
    /// the sync cannot read roles, so it is skipped with a warning.
    pub fn new(
        routes: Vec<GroupSyncRoute>,
        links: Arc<LinkBook>,
        membership: Option<Arc<MembershipChecker>>,
        messenger: Arc<dyn ServerMessenger>,
    ) -> Result<Option<Arc<Self>>, String> {
        if routes.is_empty() {
            return Ok(None);
        }
        let Some(membership) = membership else {
            tracing::warn!(
                "Discord group sync is configured but the bot token is missing; group sync is disabled"
            );
            return Ok(None);
        };
        let channel = ChannelId::modern(CHANNEL).map_err(|err| err.to_string())?;
        let routes = routes
            .into_iter()
            .map(|route| (route.server.clone(), route))
            .collect();
        Ok(Some(Arc::new(Self {
            routes,
            links,
            membership,
            messenger,
            channel,
            last_issued: Mutex::new(HashMap::new()),
            limiter: SlidingWindow::new(SYNCS_PER_WINDOW, SYNC_WINDOW),
        })))
    }

    /// Syncs a player when they reach a group-sync backend.
    pub fn subscribe(self: &Arc<Self>, bus: &dyn EventBus) -> ListenerHandle {
        let sync = Arc::clone(self);
        bus.subscribe_async::<ServerPostConnectEvent, _>(EventPriority::NORMAL, move |event| {
            let sync = Arc::clone(&sync);
            let server = event.server.clone();
            let player = Arc::clone(&event.player);
            Box::pin(async move {
                if sync.handles(server.as_str()) {
                    sync.sync_player(&server, player.as_ref()).await;
                }
            })
        })
    }

    /// Periodically re-syncs players on each group-sync backend, so Discord
    /// role removals take effect without a reconnect.
    pub async fn run_resync(
        self: Arc<Self>,
        players: Arc<dyn PlayerRegistry>,
        cancel: CancellationToken,
    ) {
        let mut due: HashMap<String, Instant> = HashMap::new();
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(RESYNC_TICK) => {}
            }
            let now = Instant::now();
            for route in self.routes.values() {
                let next = due.entry(route.server.clone()).or_insert(now);
                if *next > now {
                    continue;
                }
                *next = now + route.resync_interval;
                let server = ServerId::new(route.server.clone());
                for player in players.get_players_on_server(&server) {
                    if cancel.is_cancelled() {
                        return;
                    }
                    self.sync_player(&server, player.as_ref()).await;
                }
            }
        }
    }

    fn handles(&self, server: &str) -> bool {
        self.routes.contains_key(server)
    }

    async fn sync_player(&self, server: &ServerId, player: &dyn Player) {
        let Some(route) = self.routes.get(server.as_str()) else {
            return;
        };
        // Only Mojang-verified UUIDs may be bound to Discord roles. Offline-mode
        // UUIDs derive from the username and can be spoofed.
        if !player.is_online_mode() {
            return;
        }
        if !self.limiter.allows(server.as_str()) {
            tracing::debug!(%server, "group sync rate limit reached; skipping this player");
            return;
        }
        let uuid = player.profile().uuid;
        // A failed lookup returns `None`, which leaves the backend's groups alone.
        let Some(groups) = self.desired_groups(route, uuid).await else {
            return;
        };
        let timestamp = self.next_timestamp(uuid);
        let Some(payload) = encode(&route.secret, &route.server, uuid, timestamp, &groups) else {
            tracing::warn!(%server, "group sync payload could not be encoded");
            return;
        };
        match self
            .messenger
            .send_to_server(server, &self.channel, Bytes::from(payload))
        {
            Ok(_) => self.limiter.record(server.as_str()),
            Err(MessagingError::NoCarrier) => {
                tracing::debug!(%server, "no player is online to carry a group sync");
            }
            Err(error) => tracing::warn!(%server, error = %error, "failed to send a group sync"),
        }
    }

    /// The full set of managed groups this player should hold. Returns `None`
    /// when any role lookup fails, so the caller sends nothing.
    async fn desired_groups(&self, route: &GroupSyncRoute, uuid: Uuid) -> Option<Vec<String>> {
        let Some(discord_id) = self.links.linked_discord(uuid).await else {
            // An unlinked player holds no Discord roles, so managed groups are revoked.
            return Some(Vec::new());
        };
        let Some(user) = parse_user(&discord_id) else {
            tracing::warn!(server = %route.server, "stored Discord link is invalid; skipping group sync");
            return None;
        };
        let mut desired = BTreeSet::new();
        for (guild, role_groups) in &route.guild_roles {
            let roles = match self.membership.roles(user, *guild).await {
                Ok(Some(roles)) => roles,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(guild = %guild, error = %error, "Discord group sync role lookup failed; leaving groups unchanged");
                    return None;
                }
            };
            for (role, groups) in role_groups {
                if roles.contains(role) {
                    desired.extend(groups.iter().cloned());
                }
            }
        }
        Some(desired.into_iter().collect())
    }

    fn next_timestamp(&self, uuid: Uuid) -> i64 {
        let now = now_millis();
        let mut issued = self
            .last_issued
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if issued.len() >= MAX_TRACKED_PLAYERS && !issued.contains_key(&uuid) {
            issued.retain(|_, last| now - *last <= FRESHNESS_MS);
            if issued.len() >= MAX_TRACKED_PLAYERS {
                issued.clear();
            }
        }
        let timestamp = match issued.get(&uuid) {
            Some(last) if *last >= now => last + 1,
            _ => now,
        };
        issued.insert(uuid, timestamp);
        timestamp
    }
}

/// Encodes a sync: `IRDG`, version, type, server id, timestamp (ms), player
/// UUID, group count, length-prefixed groups, then an HMAC-SHA256 tag over
/// every preceding byte.
fn encode(
    secret: &[u8],
    server: &str,
    uuid: Uuid,
    timestamp_ms: i64,
    groups: &[String],
) -> Option<Vec<u8>> {
    let mut body = Vec::with_capacity(MAX_PAYLOAD);
    body.extend_from_slice(MAGIC);
    body.push(VERSION);
    body.push(SYNC);
    push_string(&mut body, server)?;
    body.extend_from_slice(&timestamp_ms.to_be_bytes());
    body.extend_from_slice(uuid.as_bytes());
    body.push(u8::try_from(groups.len()).ok()?);
    for group in groups {
        push_string(&mut body, group)?;
    }
    let mut mac = HmacSha256::new_from_slice(secret).ok()?;
    mac.update(&body);
    body.extend_from_slice(&mac.finalize().into_bytes());
    (body.len() <= MAX_PAYLOAD).then_some(body)
}

fn push_string(out: &mut Vec<u8>, value: &str) -> Option<()> {
    let length = u16::try_from(value.len()).ok()?;
    if length == 0 {
        return None;
    }
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(value.as_bytes());
    Some(())
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

fn parse_allowed_groups(server: &str, groups: &[String]) -> Result<BTreeSet<String>, String> {
    if groups.is_empty() || groups.len() > MAX_GROUPS {
        return Err(format!(
            "group sync for `{server}` needs 1 to {MAX_GROUPS} allowed_groups"
        ));
    }
    let mut allowed = BTreeSet::new();
    for group in groups {
        if !valid_group(group) || !allowed.insert(group.clone()) {
            return Err(format!(
                "group sync for `{server}` has an invalid or duplicate allowed group"
            ));
        }
    }
    Ok(allowed)
}

/// Lowercase LuckPerms-style names. `default` is rejected because every player
/// already inherits it.
fn valid_group(group: &str) -> bool {
    let bytes = group.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_GROUP_BYTES
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-' | b'.')
        })
        && group != "default"
}
