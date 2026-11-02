//! Opt-in Discord voice grouping from authenticated backend position snapshots.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use twilight_http::Client;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, GuildMarker};

use crate::backend_events::{PositionSnapshot, PositionStore};
use crate::config::{VoiceConfig, VoiceRoute};
use crate::membership::parse_user;
use crate::store::LinkBook;

pub(crate) struct VoiceProximity {
    config: VoiceConfig,
    links: Arc<LinkBook>,
    positions: PositionStore,
    http: Arc<Client>,
    /// Only members observed inside configured pool channels are ever moved.
    states: Mutex<HashMap<(u64, u64), u64>>,
}

impl VoiceProximity {
    pub(crate) fn new(
        config: VoiceConfig,
        links: Arc<LinkBook>,
        positions: PositionStore,
        http: Arc<Client>,
    ) -> Result<Self, String> {
        if !config.enabled {
            return Err("voice proximity is disabled".into());
        }
        if config.routes.is_empty() {
            return Err("voice proximity is enabled but has no routes".into());
        }
        let mut seen = std::collections::HashSet::new();
        for route in &config.routes {
            if route.server.trim().is_empty() || route.guild_id == 0 {
                return Err("voice routes require a server name and nonzero guild ID".into());
            }
            if route.voice_channel_ids.is_empty() || route.voice_channel_ids.len() > 64 {
                return Err(format!(
                    "voice route for `{}` needs 1 to 64 dedicated voice channels",
                    route.server
                ));
            }
            if route.radius_blocks <= 0.0 || !route.radius_blocks.is_finite() {
                return Err(format!(
                    "voice route for `{}` has an invalid radius",
                    route.server
                ));
            }
            for id in &route.voice_channel_ids {
                if *id == 0 || !seen.insert((route.guild_id, *id)) {
                    return Err("voice channel IDs must be nonzero and unique per guild".into());
                }
            }
        }
        Ok(Self {
            config,
            links,
            positions,
            http,
            states: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn observes_guild(&self, guild_id: u64) -> bool {
        self.config
            .routes
            .iter()
            .any(|route| route.guild_id == guild_id)
    }

    pub(crate) async fn update_state(&self, guild_id: u64, user_id: u64, channel_id: Option<u64>) {
        self.track_state(guild_id, user_id, channel_id);
        self.reconcile().await;
    }

    pub(crate) fn track_state(&self, guild_id: u64, user_id: u64, channel_id: Option<u64>) {
        {
            let mut states = self.states.lock().unwrap_or_else(PoisonError::into_inner);
            let route = self.config.routes.iter().find(|route| {
                route.guild_id == guild_id
                    && route
                        .voice_channel_ids
                        .iter()
                        .any(|candidate| Some(*candidate) == channel_id)
            });
            match (route, channel_id) {
                (Some(_), Some(channel)) => {
                    states.insert((guild_id, user_id), channel);
                }
                _ => {
                    states.remove(&(guild_id, user_id));
                }
            }
        }
    }

    pub(crate) async fn reconcile(&self) {
        let states = self
            .states
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let positions = self.positions.snapshot();
        for route in &self.config.routes {
            self.reconcile_route(route, &states, &positions).await;
        }
    }

    async fn reconcile_route(
        &self,
        route: &VoiceRoute,
        states: &HashMap<(u64, u64), u64>,
        positions: &[PositionSnapshot],
    ) {
        let mut players = Vec::new();
        for ((guild_id, user_id), current_channel) in states {
            if *guild_id != route.guild_id || !route.voice_channel_ids.contains(current_channel) {
                continue;
            }
            let Some(discord_user) = parse_user(&user_id.to_string()) else {
                continue;
            };
            let Some(uuid) = self.links.linked_uuid(&user_id.to_string()).await else {
                continue;
            };
            if let Some(position) = positions.iter().find(|position| {
                position.player_uuid == uuid && position.server.as_str() == route.server
            }) {
                players.push((discord_user, *current_channel, position.clone()));
            }
        }
        if players.is_empty() || players.len() > 256 {
            return;
        }

        // Form proximity components in the same world. Sorting makes channel
        // assignments deterministic while leaving excess groups untouched.
        let mut parent: Vec<usize> = (0..players.len()).collect();
        for left in 0..players.len() {
            for right in left + 1..players.len() {
                if nearby(&players[left].2, &players[right].2, route.radius_blocks) {
                    union(&mut parent, left, right);
                }
            }
        }
        let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
        for index in 0..players.len() {
            let root = find(&mut parent, index);
            groups.entry(root).or_default().push(index);
        }
        let mut groups: Vec<_> = groups.into_values().collect();
        groups.sort_by_key(|group| {
            group
                .iter()
                .map(|index| players[*index].2.player_uuid)
                .min()
        });

        let guild = Id::<GuildMarker>::new(route.guild_id);
        for (channel, group) in route.voice_channel_ids.iter().zip(groups) {
            for index in group {
                let (user, current, _) = &players[index];
                if current == channel {
                    continue;
                }
                if let Err(error) = self
                    .http
                    .update_guild_member(guild, *user)
                    .channel_id(Some(Id::<ChannelMarker>::new(*channel)))
                    .await
                {
                    tracing::warn!(guild_id = route.guild_id, user_id = %user, error = %error, "Could not move linked Discord member to proximity voice channel");
                }
            }
        }
    }
}

fn nearby(left: &PositionSnapshot, right: &PositionSnapshot, radius: f64) -> bool {
    if left.server != right.server || left.world != right.world {
        return false;
    }
    let dx = left.x - right.x;
    let dy = left.y - right.y;
    let dz = left.z - right.z;
    dx * dx + dy * dy + dz * dz <= radius * radius
}

fn find(parent: &mut [usize], index: usize) -> usize {
    if parent[index] != index {
        parent[index] = find(parent, parent[index]);
    }
    parent[index]
}

fn union(parent: &mut [usize], left: usize, right: usize) {
    let left = find(parent, left);
    let right = find(parent, right);
    if left != right {
        parent[right] = left;
    }
}
