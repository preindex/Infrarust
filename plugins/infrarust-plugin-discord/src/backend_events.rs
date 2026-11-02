//! Authenticated receiver for the optional Paper Discord companion.
//!
//! Backend secrets and Discord destinations live in a separate file so this
//! receiver does not race edits to the bot/chat configuration.

use std::collections::{HashMap, VecDeque};
use std::io::{Cursor, Read};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::backend_groups::{GroupSyncConfig, GroupSyncRoute};
use hmac::{Hmac, KeyInit, Mac};
use infrarust_api::event::bus::{EventBus, EventBusExt};
use infrarust_api::event::{BoxFuture, EventPriority, ListenerHandle};
use infrarust_api::events::PluginMessageEvent;
use infrarust_api::messaging::Endpoint;
use infrarust_api::types::ServerId;
use serde::Deserialize;
use sha2::Sha256;
use tokio::sync::Semaphore;
use twilight_http::Client;
use twilight_model::channel::message::AllowedMentions;
use twilight_model::id::Id;
use twilight_model::id::marker::ChannelMarker;
use uuid::Uuid;

const CHANNEL: &str = "infrarust:discord";
const MAGIC: &[u8; 4] = b"IRDC";
const VERSION: u8 = 1;
const DEATH: u8 = 1;
const ADVANCEMENT: u8 = 2;
const POSITION: u8 = 3;
const MAC_SIZE: usize = 32;
const MAX_PAYLOAD: usize = 512;
const MAX_STRING: usize = 128;
const MAX_HORIZONTAL_COORDINATE: f64 = 30_000_000.0;
const MAX_VERTICAL_COORDINATE: f64 = 4096.0;
const FRESHNESS: Duration = Duration::from_secs(30);
const FUTURE_SKEW: Duration = Duration::from_secs(5);
const RATE_WINDOW: Duration = Duration::from_secs(60);
const MAX_MESSAGES_PER_PLAYER_WINDOW: usize = 60;
const MAX_RATE_KEYS: usize = 8192;
const MAX_POSITIONS: usize = 4096;
const POSITION_TTL: Duration = Duration::from_secs(15);
const MAX_CONCURRENT_DISCORD_SENDS: usize = 16;

type HmacSha256 = Hmac<Sha256>;

const DEFAULT_CONFIG: &str = r#"# Optional Paper companion receiver settings. Keep this file private.
# Give each backend a unique 32-byte random secret (64 hexadecimal characters).
# The matching secret belongs in that backend's Paper plugin config.yml.
# Discord destinations receive only alerts; player UUIDs and coordinates stay proxy-side.

# [[backends]]
# server = "survival"
# shared_secret = "<64 hex characters>"
# death_channel_id = 123456789012345678
# advancement_channel_id = 123456789012345678
# positions_enabled = true
#
# Optional, opt-in: map linked Discord roles to permission groups on this
# backend. Only allowed_groups are managed, and the Paper companion removes only
# groups it granted itself. The companion must list the same allowed-groups.
#
# [backends.group_sync]
# enabled = true
# allowed_groups = ["vip", "builder"]
# resync_interval_seconds = 600
#
# [backends.group_sync.guild_roles."123456789012345678"]
# "234567890123456789" = ["vip"]
# "345678901234567890" = ["builder"]
"#;

#[derive(Deserialize, Default)]
#[serde(default)]
struct BackendEventsConfig {
    backends: Vec<BackendConfig>,
}

#[derive(Deserialize)]
struct BackendConfig {
    server: String,
    shared_secret: String,
    death_channel_id: Option<u64>,
    advancement_channel_id: Option<u64>,
    #[serde(default)]
    positions_enabled: bool,
    #[serde(default)]
    group_sync: GroupSyncConfig,
}

struct BackendRoute {
    secret: Vec<u8>,
    death_channel: Option<Id<ChannelMarker>>,
    advancement_channel: Option<Id<ChannelMarker>>,
    positions_enabled: bool,
}

#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct PositionSnapshot {
    pub(crate) player_uuid: Uuid,
    pub(crate) server: ServerId,
    pub(crate) world: String,
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) z: f64,
    pub(crate) received_at: Instant,
}

#[derive(Clone, Default)]
pub(crate) struct PositionStore {
    entries: Arc<Mutex<HashMap<RateKey, PositionSnapshot>>>,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct RateKey {
    server: String,
    player: Uuid,
}

pub struct BackendEventReceiver {
    routes: HashMap<String, BackendRoute>,
    group_routes: Vec<GroupSyncRoute>,
    http: Option<Arc<Client>>,
    rate: Mutex<HashMap<RateKey, VecDeque<Instant>>>,
    positions: PositionStore,
    discord_sends: Arc<Semaphore>,
}

impl BackendEventReceiver {
    pub async fn load(path: &std::path::Path, http: Option<Arc<Client>>) -> Result<Self, String> {
        let contents = match tokio::fs::read_to_string(path).await {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|error| error.to_string())?;
                }
                tokio::fs::write(path, DEFAULT_CONFIG)
                    .await
                    .map_err(|error| error.to_string())?;
                DEFAULT_CONFIG.to_owned()
            }
            Err(error) => return Err(error.to_string()),
        };
        let config: BackendEventsConfig = toml::from_str(&contents)
            .map_err(|error| format!("backend-events.toml is invalid: {error}"))?;
        let mut routes = HashMap::new();
        let mut group_routes = Vec::new();
        let mut seen_secrets = std::collections::HashSet::new();
        for backend in config.backends {
            let server = backend.server.trim();
            if server.is_empty() {
                return Err("backend-events.toml has an empty backend server id".into());
            }
            let secret = decode_secret(&backend.shared_secret).map_err(|_| {
                format!(
                    "backend-events.toml secret for `{server}` must be 64 hexadecimal characters"
                )
            })?;
            if !seen_secrets.insert(secret.clone()) {
                return Err(format!(
                    "backend-events.toml reuses a secret for `{server}`"
                ));
            }
            if let Some(group_route) = GroupSyncRoute::compile(server, &secret, &backend.group_sync)
                .map_err(|error| format!("backend-events.toml: {error}"))?
            {
                group_routes.push(group_route);
            }
            let route = BackendRoute {
                secret,
                death_channel: parse_channel_id(backend.death_channel_id, server, "death")?,
                advancement_channel: parse_channel_id(
                    backend.advancement_channel_id,
                    server,
                    "advancement",
                )?,
                positions_enabled: backend.positions_enabled,
            };
            if routes.insert(server.to_owned(), route).is_some() {
                return Err(format!("backend-events.toml repeats backend `{server}`"));
            }
        }
        Ok(Self {
            routes,
            group_routes,
            http,
            rate: Mutex::new(HashMap::new()),
            positions: PositionStore::default(),
            discord_sends: Arc::new(Semaphore::new(MAX_CONCURRENT_DISCORD_SENDS)),
        })
    }

    /// Validated group sync settings for every backend that enables them.
    pub(crate) fn group_sync_routes(&self) -> Vec<GroupSyncRoute> {
        self.group_routes.clone()
    }

    pub fn subscribe(self: &Arc<Self>, bus: &dyn EventBus) -> ListenerHandle {
        let receiver = Arc::clone(self);
        bus.subscribe_async::<PluginMessageEvent, _>(EventPriority::LAST, move |event| {
            if event.raw_channel != CHANNEL {
                return Box::pin(async {}) as BoxFuture<'_, ()>;
            }
            // This is a private backend-to-proxy channel. Never leak its bytes to a client.
            event.handled();
            let decoded = match &event.source {
                Endpoint::Backend(server) => receiver.receive(server, event),
                Endpoint::Client => Err("message source is the client"),
                _ => Err("message source is unsupported"),
            };
            let (server, payload) = match decoded {
                Ok(decoded) => decoded,
                Err(reason) => {
                    tracing::debug!(reason, "Rejected Discord companion plugin message");
                    return Box::pin(async {}) as BoxFuture<'_, ()>;
                }
            };
            let username = event.player.profile().username.clone();
            let alert = receiver.alert_message(&server, &username, payload);
            let sender = Arc::clone(&receiver);
            Box::pin(async move {
                if let Some((channel, message)) = alert {
                    sender.send_alert(channel, message).await;
                }
            })
        })
    }

    #[allow(dead_code)]
    pub(crate) fn position_store(&self) -> PositionStore {
        self.positions.clone()
    }

    fn receive(
        &self,
        server: &ServerId,
        event: &PluginMessageEvent,
    ) -> Result<(ServerId, Payload), &'static str> {
        let route = self
            .routes
            .get(server.as_str())
            .ok_or("backend is not configured")?;
        let payload = decode(&event.data, &route.secret).ok_or("invalid payload")?;
        if payload.player != event.player.profile().uuid {
            return Err("payload player does not match the message carrier");
        }
        if matches!(payload.kind, PayloadKind::Position { .. }) && !route.positions_enabled {
            return Err("positions are disabled for this backend");
        }
        let key = RateKey {
            server: server.as_str().to_owned(),
            player: payload.player,
        };
        if !self.allow(&key) {
            return Err("backend/player event rate limit exceeded");
        }
        if let PayloadKind::Position { world, x, y, z } = &payload.kind {
            self.store_position(
                key,
                server.clone(),
                payload.player,
                world.clone(),
                *x,
                *y,
                *z,
            );
        }
        Ok((server.clone(), payload))
    }

    fn allow(&self, key: &RateKey) -> bool {
        let now = Instant::now();
        let mut rate = self.rate.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(events) = rate.get_mut(key) {
            prune(events, now, RATE_WINDOW);
            if events.len() >= MAX_MESSAGES_PER_PLAYER_WINDOW {
                return false;
            }
            events.push_back(now);
            return true;
        }
        if rate.len() >= MAX_RATE_KEYS {
            rate.retain(|_, events| {
                prune(events, now, RATE_WINDOW);
                !events.is_empty()
            });
            if rate.len() >= MAX_RATE_KEYS {
                return false;
            }
        }
        rate.insert(key.clone(), VecDeque::from([now]));
        true
    }

    fn store_position(
        &self,
        key: RateKey,
        server: ServerId,
        player: Uuid,
        world: String,
        x: f64,
        y: f64,
        z: f64,
    ) {
        self.positions.insert(
            PositionSnapshot {
                player_uuid: player,
                server,
                world,
                x,
                y,
                z,
                received_at: Instant::now(),
            },
            key,
        );
    }

    fn alert_message(
        &self,
        server: &ServerId,
        username: &str,
        payload: Payload,
    ) -> Option<(Id<ChannelMarker>, String)> {
        let route = self.routes.get(server.as_str())?;
        let username = escape_display_name(username);
        let safe_server = escape_inline_code(server.as_str());
        match payload.kind {
            PayloadKind::Death => route
                .death_channel
                .map(|channel| (channel, format!("**{username} died** on `{safe_server}`."))),
            PayloadKind::Advancement(key) => route.advancement_channel.map(|channel| {
                let safe_key = escape_inline_code(&key);
                (
                    channel,
                    format!("**{username}** earned `{safe_key}` on `{safe_server}`."),
                )
            }),
            PayloadKind::Position { .. } => None,
        }
    }

    async fn send_alert(&self, channel: Id<ChannelMarker>, message: String) {
        let Some(http) = &self.http else { return };
        let Ok(permit) = Arc::clone(&self.discord_sends).try_acquire_owned() else {
            tracing::debug!("Discord companion alert queue is full; dropping alert");
            return;
        };
        let http = Arc::clone(http);
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(error) = http
                .create_message(channel)
                .content(&message)
                .allowed_mentions(Some(&AllowedMentions::default()))
                .await
            {
                tracing::warn!(error = %error, "Failed to send Discord backend alert");
            }
        });
    }
}

impl PositionStore {
    /// Returns non-expired snapshots for proxy-side voice grouping.
    /// This store is internal to the native Discord plugin.
    #[allow(dead_code)]
    pub(crate) fn snapshot(&self) -> Vec<PositionSnapshot> {
        let now = Instant::now();
        let mut positions = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        positions
            .retain(|_, entry| now.saturating_duration_since(entry.received_at) <= POSITION_TTL);
        positions.values().cloned().collect()
    }

    fn insert(&self, snapshot: PositionSnapshot, key: RateKey) {
        let now = Instant::now();
        let mut positions = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        positions
            .retain(|_, entry| now.saturating_duration_since(entry.received_at) <= POSITION_TTL);
        if !positions.contains_key(&key) && positions.len() >= MAX_POSITIONS {
            if let Some(oldest) = positions
                .iter()
                .min_by_key(|(_, entry)| entry.received_at)
                .map(|(key, _)| key.clone())
            {
                positions.remove(&oldest);
            }
        }
        positions.insert(key, snapshot);
    }
}

struct Payload {
    player: Uuid,
    kind: PayloadKind,
}

enum PayloadKind {
    Death,
    Advancement(String),
    Position {
        world: String,
        x: f64,
        y: f64,
        z: f64,
    },
}

fn decode(data: &[u8], secret: &[u8]) -> Option<Payload> {
    if data.len() > MAX_PAYLOAD || data.len() < 4 + 1 + 1 + 8 + 16 + MAC_SIZE {
        return None;
    }
    let body_len = data.len().checked_sub(MAC_SIZE)?;
    let (body, tag) = data.split_at(body_len);
    let mut mac = HmacSha256::new_from_slice(secret).ok()?;
    mac.update(body);
    mac.verify_slice(tag).ok()?;

    let mut input = Cursor::new(body);
    let mut magic = [0_u8; 4];
    input.read_exact(&mut magic).ok()?;
    if &magic != MAGIC || read_u8(&mut input)? != VERSION {
        return None;
    }
    let event_type = read_u8(&mut input)?;
    let timestamp = read_i64(&mut input)?;
    if !timestamp_is_fresh(timestamp) {
        return None;
    }
    let uuid =
        Uuid::from_u128(((read_u64(&mut input)? as u128) << 64) | read_u64(&mut input)? as u128);
    let kind = match event_type {
        DEATH => PayloadKind::Death,
        ADVANCEMENT => {
            let key = read_string(&mut input, MAX_STRING)?;
            if !valid_namespaced_key(&key) {
                return None;
            }
            PayloadKind::Advancement(key)
        }
        POSITION => {
            let world = read_string(&mut input, MAX_STRING)?;
            if !valid_namespaced_key(&world) {
                return None;
            }
            let x = read_f64(&mut input)?;
            let y = read_f64(&mut input)?;
            let z = read_f64(&mut input)?;
            if !x.is_finite()
                || !y.is_finite()
                || !z.is_finite()
                || x.abs() > MAX_HORIZONTAL_COORDINATE
                || z.abs() > MAX_HORIZONTAL_COORDINATE
                || y.abs() > MAX_VERTICAL_COORDINATE
            {
                return None;
            }
            PayloadKind::Position { world, x, y, z }
        }
        _ => return None,
    };
    if input.position() != body.len() as u64 {
        return None;
    }
    Some(Payload { player: uuid, kind })
}

fn timestamp_is_fresh(timestamp: i64) -> bool {
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return false;
    };
    let now = now.as_secs() as i64;
    timestamp >= now - FRESHNESS.as_secs() as i64 && timestamp <= now + FUTURE_SKEW.as_secs() as i64
}

fn read_string(input: &mut Cursor<&[u8]>, max: usize) -> Option<String> {
    let length = read_u16(input)? as usize;
    if length == 0 || length > max || remaining(input) < length {
        return None;
    }
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes).ok()?;
    String::from_utf8(bytes).ok()
}

fn read_u8(input: &mut Cursor<&[u8]>) -> Option<u8> {
    let mut bytes = [0; 1];
    input.read_exact(&mut bytes).ok()?;
    Some(bytes[0])
}

fn read_u16(input: &mut Cursor<&[u8]>) -> Option<u16> {
    let mut bytes = [0; 2];
    input.read_exact(&mut bytes).ok()?;
    Some(u16::from_be_bytes(bytes))
}

fn read_u64(input: &mut Cursor<&[u8]>) -> Option<u64> {
    let mut bytes = [0; 8];
    input.read_exact(&mut bytes).ok()?;
    Some(u64::from_be_bytes(bytes))
}

fn read_i64(input: &mut Cursor<&[u8]>) -> Option<i64> {
    let mut bytes = [0; 8];
    input.read_exact(&mut bytes).ok()?;
    Some(i64::from_be_bytes(bytes))
}

fn read_f64(input: &mut Cursor<&[u8]>) -> Option<f64> {
    Some(f64::from_bits(read_u64(input)?))
}

fn remaining(input: &Cursor<&[u8]>) -> usize {
    input
        .get_ref()
        .len()
        .saturating_sub(input.position() as usize)
}

fn decode_secret(value: &str) -> Result<Vec<u8>, ()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(());
    }
    (0..64)
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).map_err(|_| ()))
        .collect()
}

fn valid_namespaced_key(value: &str) -> bool {
    let Some((namespace, path)) = value.split_once(':') else {
        return false;
    };
    !namespace.is_empty()
        && !path.is_empty()
        && namespace.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
        && path.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-/".contains(&byte)
        })
}

fn parse_channel_id(
    value: Option<u64>,
    server: &str,
    kind: &str,
) -> Result<Option<Id<ChannelMarker>>, String> {
    value
        .map(|id| {
            if id == 0 {
                Err(format!("{kind} channel id for `{server}` must be nonzero"))
            } else {
                Ok(Id::new(id))
            }
        })
        .transpose()
}

fn prune(queue: &mut VecDeque<Instant>, now: Instant, window: Duration) {
    while queue
        .front()
        .is_some_and(|front| now.saturating_duration_since(*front) >= window)
    {
        queue.pop_front();
    }
}

fn escape_inline_code(value: &str) -> String {
    value
        .replace('`', "ˋ")
        .replace('@', "@\u{200b}")
        .chars()
        .take(MAX_STRING)
        .collect()
}

fn escape_display_name(value: &str) -> String {
    value
        .replace('@', "@\u{200b}")
        .replace('*', "\\*")
        .replace('_', "\\_")
        .replace('~', "\\~")
        .replace('|', "\\|")
        .chars()
        .take(64)
        .collect()
}
