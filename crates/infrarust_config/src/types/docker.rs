//! Docker provider configuration.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::defaults;

/// What the Docker provider discovers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DockerMode {
    /// Standalone containers labelled `infrarust.*`.
    #[default]
    Container,
    /// Swarm services labelled `infrarust.*` in `Spec.Labels`, routed through
    /// the service VIP on the overlay named by `network`.
    Swarm,
}

/// This type is always compiled (no feature gate) so that
/// `ProxyConfig` can parse a `[docker]` section regardless of
/// the build configuration. The `DockerProvider` itself
/// is feature-gated in `infrarust-core`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerProviderConfig {
    /// Endpoint Docker (ex: "<unix:///var/run/docker.sock>").
    #[serde(default = "defaults::docker_endpoint")]
    pub endpoint: String,

    /// Discovery mode: standalone `container` (default) or `swarm`.
    #[serde(default)]
    pub mode: DockerMode,

    /// Preferred Docker network for address resolution.
    /// In `swarm` mode this is the overlay network whose service VIP is
    /// used as the backend address, and it is required.
    #[serde(default)]
    pub network: Option<String>,

    /// Fallback polling interval.
    #[serde(default = "defaults::docker_poll_interval")]
    #[serde(with = "humantime_serde")]
    pub poll_interval: Duration,

    /// Reconnection delay after Docker daemon disconnection.
    #[serde(default = "defaults::docker_reconnect_delay")]
    #[serde(with = "humantime_serde")]
    pub reconnect_delay: Duration,
}

impl Default for DockerProviderConfig {
    fn default() -> Self {
        Self {
            endpoint: defaults::docker_endpoint(),
            mode: DockerMode::default(),
            network: None,
            poll_interval: defaults::docker_poll_interval(),
            reconnect_delay: defaults::docker_reconnect_delay(),
        }
    }
}
