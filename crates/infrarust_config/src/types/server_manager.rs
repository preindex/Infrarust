//! Server manager configuration (auto start/stop).

use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};

use crate::defaults;

/// Server manager configuration (auto start/stop).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerManagerConfig {
    Local(LocalManagerConfig),
    Pterodactyl(PterodactylManagerConfig),
    Crafty(CraftyManagerConfig),
    DockerSwarm(DockerSwarmManagerConfig),
}

/// Docker Swarm singleton service, scaled between 0 and 1 replica.
///
/// The service is addressed by its immutable Swarm ID. Only replicated
/// services are accepted; the manager never scales anything but a singleton.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerSwarmManagerConfig {
    /// Swarm manager endpoint (`unix://...`, `tcp://...` or `http://...`).
    #[serde(default = "defaults::docker_endpoint")]
    pub endpoint: String,
    /// Immutable Swarm service ID. Service names can be reused, IDs cannot.
    pub service_id: String,
    /// Minecraft port served by the service VIP (the container's target port).
    #[serde(default = "defaults::swarm_game_port")]
    pub game_port: u16,
    /// Overlay network whose VIP is probed. The first VIP is used when unset.
    #[serde(default)]
    pub network_id: Option<String>,
    /// Lowest replica count: `0` (scale to zero when idle) or `1` (always on).
    #[serde(default, deserialize_with = "deserialize_min_replicas")]
    pub min_replicas: u8,
    /// Highest replica count. Must be `1`: stateful Minecraft servers cannot run
    /// more than one replica against the same world.
    #[serde(
        default = "defaults::swarm_max_replicas",
        deserialize_with = "deserialize_max_replicas"
    )]
    pub max_replicas: u8,
    /// Idle duration before scale-to-zero (None = disabled).
    #[serde(default)]
    #[serde(with = "humantime_serde")]
    pub shutdown_after: Option<Duration>,
    /// Minimum time a started replica stays up before idle scale-down may run.
    /// Independent of `shutdown_after`.
    #[serde(default)]
    #[serde(with = "humantime_serde")]
    pub min_uptime: Option<Duration>,
    /// Time after a scale-down before the service may be scaled up again.
    /// Independent of `shutdown_after`.
    #[serde(default)]
    #[serde(with = "humantime_serde")]
    pub cooldown: Option<Duration>,
    /// Wake timeout: how long waiting players wait for the server to come online.
    #[serde(default = "defaults::start_timeout")]
    #[serde(with = "humantime_serde")]
    pub start_timeout: Duration,
    /// Upper bound for the stop wait until old tasks reach a terminal state.
    #[serde(default = "defaults::shutdown_timeout")]
    #[serde(with = "humantime_serde")]
    pub shutdown_timeout: Duration,
    /// Readiness: timeout for each Minecraft status ping to the service VIP.
    #[serde(default = "defaults::swarm_readiness_timeout")]
    #[serde(with = "humantime_serde")]
    pub readiness_timeout: Duration,
    #[serde(default = "defaults::poll_interval")]
    #[serde(with = "humantime_serde")]
    pub poll_interval: Duration,
}

fn deserialize_min_replicas<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u8, D::Error> {
    let value = u8::deserialize(deserializer)?;
    if value > 1 {
        return Err(serde::de::Error::custom(format!(
            "docker swarm min_replicas must be 0 or 1, got {value}"
        )));
    }
    Ok(value)
}

fn deserialize_max_replicas<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u8, D::Error> {
    let value = u8::deserialize(deserializer)?;
    if value != 1 {
        return Err(serde::de::Error::custom(format!(
            "docker swarm max_replicas must be 1, got {value}: stateful servers cannot scale out"
        )));
    }
    Ok(value)
}

/// Local provider: launches a local Java process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalManagerConfig {
    /// Command to execute (e.g., "java")
    pub command: String,
    /// Working directory
    pub working_dir: std::path::PathBuf,
    /// Arguments (ex: `["-Xmx4G", "-jar", "server.jar", "nogui"]`)
    #[serde(default)]
    pub args: Vec<String>,
    /// Pattern in stdout indicating the server is ready
    #[serde(default = "defaults::ready_pattern")]
    pub ready_pattern: String,
    /// Timeout for graceful shutdown
    #[serde(default = "defaults::shutdown_timeout")]
    #[serde(with = "humantime_serde")]
    pub shutdown_timeout: Duration,
    /// Idle duration before automatic shutdown (None = disabled)
    #[serde(default)]
    #[serde(with = "humantime_serde")]
    pub shutdown_after: Option<Duration>,
    /// Timeout for server startup
    #[serde(default = "defaults::start_timeout")]
    #[serde(with = "humantime_serde")]
    pub start_timeout: Duration,
    #[serde(default = "defaults::poll_interval")]
    #[serde(with = "humantime_serde")]
    pub poll_interval: Duration,
}

macro_rules! rest_manager_config {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct $name {
            pub api_url: String,
            pub api_key: String,
            pub server_id: String,
            #[serde(default)]
            #[serde(with = "humantime_serde")]
            pub shutdown_after: Option<Duration>,
            #[serde(default = "defaults::start_timeout")]
            #[serde(with = "humantime_serde")]
            pub start_timeout: Duration,
            #[serde(default = "defaults::poll_interval")]
            #[serde(with = "humantime_serde")]
            pub poll_interval: Duration,
        }
    };
}

rest_manager_config!(PterodactylManagerConfig);
rest_manager_config!(CraftyManagerConfig);
