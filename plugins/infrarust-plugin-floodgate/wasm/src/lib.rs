//! Infrarust WASM adapter for the Floodgate identity codec.

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::rc::Rc;
use std::time::{Duration, Instant};

use infrarust_plugin_floodgate::{BedrockIdentity, IdentityOptions, decode_handshake};
use infrarust_plugin_sdk::prelude::*;
use ipnet::IpNet;
use serde::Deserialize;

const SETTINGS_FILE: &str = "config.toml";
const KEY_LENGTH: usize = 16;
const MAX_TTL_SECONDS: u64 = 3_600;
const MAX_CACHE_ENTRIES: usize = 65_536;
const MARKER_PREFIX: &str = "^Floodgate^";
const REJECT_MESSAGE: &str = "Floodgate login could not be verified";

#[derive(Default)]
struct FloodgatePlugin;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    key_file: String,
    trusted_sources: Vec<IpNet>,
    #[serde(default = "default_prefix")]
    username_prefix: String,
    #[serde(default = "default_true")]
    replace_spaces: bool,
    #[serde(default = "default_pending_ttl")]
    pending_ttl_seconds: u64,
    #[serde(default = "default_replay_ttl")]
    replay_ttl_seconds: u64,
    #[serde(default = "default_pending_capacity")]
    max_pending: usize,
    #[serde(default = "default_replay_capacity")]
    max_replays: usize,
}

fn default_prefix() -> String {
    ".".to_owned()
}

const fn default_true() -> bool {
    true
}

const fn default_pending_ttl() -> u64 {
    30
}

const fn default_replay_ttl() -> u64 {
    3_600
}

const fn default_pending_capacity() -> usize {
    4_096
}

const fn default_replay_capacity() -> usize {
    16_384
}

impl Settings {
    fn load() -> Result<(Self, Vec<u8>, IdentityOptions), PluginError> {
        let raw = std::fs::read_to_string(SETTINGS_FILE).map_err(|_| {
            PluginError::new("Floodgate plugin config.toml is missing or unreadable")
        })?;
        let settings: Self = toml::from_str(&raw)
            .map_err(|_| PluginError::new("Floodgate plugin config.toml is invalid"))?;
        if settings.trusted_sources.is_empty() {
            return Err(PluginError::new(
                "Floodgate requires at least one trusted_sources CIDR",
            ));
        }
        if !(1..=MAX_TTL_SECONDS).contains(&settings.pending_ttl_seconds)
            || !(1..=MAX_TTL_SECONDS).contains(&settings.replay_ttl_seconds)
        {
            return Err(PluginError::new(
                "Floodgate TTL must be between 1 and 3600 seconds",
            ));
        }
        if !(1..=MAX_CACHE_ENTRIES).contains(&settings.max_pending)
            || !(1..=MAX_CACHE_ENTRIES).contains(&settings.max_replays)
        {
            return Err(PluginError::new(
                "Floodgate cache capacities must be between 1 and 65536",
            ));
        }

        let key = std::fs::read(&settings.key_file)
            .map_err(|_| PluginError::new("Floodgate key file is missing or unreadable"))?;
        if key.len() != KEY_LENGTH {
            return Err(PluginError::new(
                "Floodgate key file must contain exactly 16 raw bytes",
            ));
        }

        let options = IdentityOptions {
            username_prefix: settings.username_prefix.clone(),
            replace_spaces: settings.replace_spaces,
        };
        // Validate identity options at startup, before accepting logins.
        if options.username_prefix.chars().count() >= 16 {
            return Err(PluginError::new(
                "Floodgate username_prefix must be shorter than 16 characters",
            ));
        }

        Ok((settings, key, options))
    }

    fn trusts(&self, ip: IpAddr) -> bool {
        self.trusted_sources.iter().any(|net| net.contains(&ip))
    }
}

#[derive(Clone)]
enum PendingLogin {
    Rejected {
        expires_at: Instant,
    },
    Verified {
        identity: BedrockIdentity,
        expires_at: Instant,
        prelogin_authorized: bool,
    },
}

impl PendingLogin {
    fn expires_at(&self) -> Instant {
        match self {
            Self::Rejected { expires_at } | Self::Verified { expires_at, .. } => *expires_at,
        }
    }
}

#[derive(Default)]
struct Connections {
    pending: BTreeMap<SocketAddr, PendingLogin>,
    replayed_ivs: BTreeMap<[u8; 12], Instant>,
}

impl Connections {
    fn prune(&mut self, now: Instant) {
        self.pending.retain(|_, record| record.expires_at() > now);
        self.replayed_ivs.retain(|_, expires_at| *expires_at > now);
    }

    fn mark_rejected(&mut self, address: SocketAddr, expires_at: Instant, max_pending: usize) {
        if self.pending.len() < max_pending || self.pending.contains_key(&address) {
            self.pending
                .insert(address, PendingLogin::Rejected { expires_at });
        }
    }
}

#[plugin(id = "floodgate", name = "Floodgate Compatibility")]
impl Plugin for FloodgatePlugin {
    fn on_enable(&self, ctx: &Context) -> Result<(), PluginError> {
        // beta.4's event remote_addr reflects the PROXY header when enabled.
        // Do not use that spoofable address as the correlation or trust key.
        if Proxy::details().receive_proxy_protocol {
            return Err(PluginError::new(
                "Floodgate requires receive_proxy_protocol = false",
            ));
        }

        let (settings, key, identity_options) = Settings::load()?;
        let pending_ttl = Duration::from_secs(settings.pending_ttl_seconds);
        let replay_ttl = Duration::from_secs(settings.replay_ttl_seconds);
        let max_pending = settings.max_pending;
        let max_replays = settings.max_replays;
        let shared = Rc::new(RefCell::new(Connections::default()));

        let handshake_state = Rc::clone(&shared);
        let handshake_key = key;
        let handshake_settings = settings;
        let handshake_options = identity_options;
        ctx.on::<ConnectionHandshakeEvent>(EventPriority::LAST, move |event| {
            let address = event.remote_addr;
            let now = Instant::now();
            let mut state = handshake_state.borrow_mut();
            state.prune(now);
            // A new handshake from a reused source socket replaces any stale
            // entry before its hostname is considered.
            state.pending.remove(&address);

            let marked = event
                .raw_host
                .split('\0')
                .any(|segment| segment.starts_with(MARKER_PREFIX));
            if !marked {
                return;
            }

            let expires_at = now + pending_ttl;
            if event.intent != HandshakeIntent::Login {
                event.deny(Component::text(REJECT_MESSAGE));
                return;
            }
            if !handshake_settings.trusts(address.ip()) {
                state.mark_rejected(address, expires_at, max_pending);
                event.deny(Component::text(REJECT_MESSAGE));
                return;
            }

            match decode_handshake(&event.raw_host, &handshake_key, &handshake_options) {
                Ok(Some(identity)) => {
                    state.prune(now);
                    let iv = *identity.replay_iv();
                    if state.replayed_ivs.contains_key(&iv) {
                        state.mark_rejected(address, expires_at, max_pending);
                        event.deny(Component::text(REJECT_MESSAGE));
                        return;
                    }
                    if state.replayed_ivs.len() >= max_replays {
                        state.mark_rejected(address, expires_at, max_pending);
                        event.deny(Component::text(REJECT_MESSAGE));
                        return;
                    }
                    if state.pending.len() >= max_pending {
                        event.deny(Component::text(REJECT_MESSAGE));
                        return;
                    }
                    state.replayed_ivs.insert(iv, now + replay_ttl);
                    state.pending.insert(
                        address,
                        PendingLogin::Verified {
                            identity,
                            expires_at,
                            prelogin_authorized: false,
                        },
                    );
                }
                Ok(None) | Err(_) => {
                    state.mark_rejected(address, expires_at, max_pending);
                    event.deny(Component::text(REJECT_MESSAGE));
                }
            }
        })?;

        let prelogin_state = Rc::clone(&shared);
        ctx.on::<PreLoginEvent>(EventPriority::LAST, move |event| {
            if matches!(event.result(), PreLoginResult::Denied(_)) {
                return;
            }
            let now = Instant::now();
            let mut state = prelogin_state.borrow_mut();
            state.prune(now);
            match state.pending.get_mut(&event.remote_addr) {
                Some(PendingLogin::Rejected { .. }) => {
                    event.deny(Component::text(REJECT_MESSAGE));
                }
                Some(PendingLogin::Verified {
                    identity,
                    prelogin_authorized,
                    ..
                }) => {
                    if !login_name_matches(&event.profile.username, identity) {
                        state.pending.remove(&event.remote_addr);
                        event.deny(Component::text(REJECT_MESSAGE));
                        return;
                    }
                    // This event result is scoped to the verified socket; Java
                    // clients without handshake evidence retain their result.
                    *prelogin_authorized = true;
                    event.force_offline();
                }
                None => {
                    // Refuse other plugins' offline-auth bypasses on this
                    // supported client_only path unless Floodgate verified it.
                    if matches!(event.result(), PreLoginResult::ForceOffline) {
                        event.deny(Component::text(REJECT_MESSAGE));
                    }
                }
            }
        })?;

        let profile_state = shared;
        ctx.on::<GameProfileRequestEvent>(EventPriority::LAST, move |event| {
            let now = Instant::now();
            let mut state = profile_state.borrow_mut();
            state.prune(now);
            let address = event.remote_addr;
            let Some(record) = state.pending.get(&address).cloned() else {
                if !event.online_mode {
                    event.deny(Component::text(REJECT_MESSAGE));
                }
                return;
            };

            match record {
                PendingLogin::Rejected { .. } => {
                    state.pending.remove(&address);
                    event.deny(Component::text(REJECT_MESSAGE));
                }
                PendingLogin::Verified {
                    identity,
                    prelogin_authorized: true,
                    ..
                } if !event.online_mode
                    && login_name_matches(&event.original.username, &identity) =>
                {
                    let mut profile = event.profile().clone();
                    profile.uuid = identity.uuid();
                    profile.username = identity.java_username().to_owned();
                    // Mojang textures from another profile must never be
                    // attached to this authenticated XUID identity.
                    profile.properties.clear();
                    event.set_profile(profile);
                    state.pending.remove(&address);
                }
                _ => {
                    state.pending.remove(&address);
                    event.deny(Component::text(REJECT_MESSAGE));
                }
            }
        })?;

        info!("[floodgate] verified-login adapter enabled");
        Ok(())
    }
}

fn login_name_matches(candidate: &str, identity: &BedrockIdentity) -> bool {
    if candidate == identity.bedrock_username() || candidate == identity.java_username() {
        return true;
    }
    candidate == identity.bedrock_username().replace(' ', "_")
}
