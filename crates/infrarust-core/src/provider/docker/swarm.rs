//! Swarm service discovery: pure snapshot building.
//!
//! A snapshot is built from typed manager API responses (labelled services,
//! their running task counts, and the configured overlay network id). Each
//! routable service is registered at its VIP on that overlay, so routing never
//! depends on per-task addresses (the typed `Task` model carries no network
//! attachments). Services that cannot be routed safely are reported as skipped
//! with a reason instead of being registered.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

use bollard::models::{
    EndpointSpecModeEnum, Service, ServiceSpecMode, ServiceSpecUpdateConfigOrderEnum, Task,
    TaskState,
};

use infrarust_config::{
    DEFAULT_MC_PORT, DockerSwarmManagerConfig, ServerConfig, ServerManagerConfig,
};

use super::labels::service_labels_to_server_config;
use crate::provider::{ProviderConfig, ProviderEvent, ProviderId};

/// Prefix of the unique id of a swarm service, e.g. `swarm:<service-id>`.
/// The full `ProviderId` renders as `docker@swarm:<service-id>`.
pub const SWARM_KEY_PREFIX: &str = "swarm:";

/// Label that opts a service into discovery.
pub const ENABLE_LABEL: &str = "infrarust.enable=true";

/// Why a labelled service is not registered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SkipReason {
    #[error("service has no replicated, global, or job mode")]
    UnknownMode,
    #[error("global services have one task per node and no single backend")]
    Global,
    #[error("replicated jobs and global jobs are not supported")]
    Job,
    #[error("dnsrr endpoint mode has no VIP to route to")]
    DnsRr,
    #[error("start-first updates can overlap two backends on one service")]
    StartFirst,
    #[error("service has {replicas} replicas; only 0 or 1 are supported")]
    MultipleReplicas { replicas: i64 },
    #[error("service replica count is unknown")]
    UnknownReplicas,
    #[error("service has {running} running tasks; only one backend is supported")]
    MultipleRunningTasks { running: usize },
    #[error("service has no VIP on the configured overlay network")]
    NotOnOverlay,
    #[error("service has no id")]
    MissingId,
    #[error("invalid server config: {0}")]
    InvalidConfig(String),
}

/// A labelled service that was not registered, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedService {
    /// Docker service id.
    pub service_id: String,
    /// Service name, or the id when the spec has no name.
    pub service_name: String,
    pub reason: SkipReason,
}

/// Result of one complete swarm snapshot.
#[derive(Debug, Default)]
pub struct SwarmSnapshot {
    /// Routable services, one per service id.
    pub routes: Vec<ProviderConfig>,
    /// Labelled services that were not registered.
    pub skipped: Vec<SkippedService>,
}

/// Diffs a complete snapshot against the known routes and updates `known`.
///
/// Only added, changed, or removed routes produce events, so a steady poll is
/// silent. Call this only with a snapshot that succeeded in full: a partial
/// listing would remove routes that are still live.
pub fn diff_routes(
    known: &mut HashMap<String, ServerConfig>,
    current: Vec<ProviderConfig>,
) -> Vec<ProviderEvent> {
    let mut events = Vec::new();
    let current: HashMap<String, ProviderConfig> = current
        .into_iter()
        .map(|pc| (pc.id.unique_id.clone(), pc))
        .collect();

    for (key, pc) in &current {
        let previous = known.get(key);
        if previous == Some(&pc.config) {
            continue;
        }
        let event = if previous.is_some() {
            ProviderEvent::Updated(pc.clone())
        } else {
            ProviderEvent::Added(pc.clone())
        };
        events.push(event);
        known.insert(key.clone(), pc.config.clone());
    }

    let removed: Vec<String> = known
        .keys()
        .filter(|key| !current.contains_key(*key))
        .cloned()
        .collect();
    for key in removed {
        known.remove(&key);
        events.push(ProviderEvent::Removed(ProviderId::docker(key)));
    }

    events
}

/// Counts running tasks per service id from a typed task listing.
pub fn count_running_tasks(tasks: &[Task]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for task in tasks {
        let running = matches!(
            task.status.as_ref().and_then(|s| s.state.as_ref()),
            Some(TaskState::RUNNING)
        );
        if running && let Some(service_id) = &task.service_id {
            *counts.entry(service_id.clone()).or_insert(0) += 1;
        }
    }
    counts
}

/// Builds the snapshot for every labelled service.
///
/// `running_tasks` maps service id to its running task count, and `overlay_id`
/// is the id of the configured overlay network. Services without
/// `infrarust.enable=true` are ignored silently.
pub fn build_snapshot(
    services: &[Service],
    running_tasks: &HashMap<String, usize>,
    overlay_id: &str,
) -> SwarmSnapshot {
    build_snapshot_with_endpoint(
        services,
        running_tasks,
        overlay_id,
        None,
        std::time::Duration::from_secs(5),
    )
}

/// Builds a snapshot and attaches manager config to services that explicitly
/// opt in with `infrarust.swarm.scale=true`.
pub fn build_snapshot_with_endpoint(
    services: &[Service],
    running_tasks: &HashMap<String, usize>,
    overlay_id: &str,
    docker_endpoint: Option<&str>,
    poll_interval: std::time::Duration,
) -> SwarmSnapshot {
    let mut snapshot = SwarmSnapshot::default();

    for service in services {
        let labelled = service
            .spec
            .as_ref()
            .and_then(|spec| spec.labels.as_ref())
            .and_then(|labels| labels.get("infrarust.enable"))
            .is_some_and(|v| v == "true");
        if !labelled {
            continue;
        }

        let service_id = service.id.clone().unwrap_or_default();
        let service_name = service
            .spec
            .as_ref()
            .and_then(|spec| spec.name.clone())
            .unwrap_or_else(|| service_id.clone());

        match service_config(
            service,
            &service_id,
            &service_name,
            running_tasks,
            overlay_id,
            docker_endpoint,
            poll_interval,
        ) {
            Ok(pc) => snapshot.routes.push(pc),
            Err(reason) => snapshot.skipped.push(SkippedService {
                service_id,
                service_name,
                reason,
            }),
        }
    }

    snapshot
}

fn service_config(
    service: &Service,
    service_id: &str,
    service_name: &str,
    running_tasks: &HashMap<String, usize>,
    overlay_id: &str,
    docker_endpoint: Option<&str>,
    poll_interval: std::time::Duration,
) -> Result<ProviderConfig, SkipReason> {
    if service_id.is_empty() {
        return Err(SkipReason::MissingId);
    }
    let spec = service.spec.as_ref().ok_or(SkipReason::UnknownMode)?;

    check_mode(spec.mode.as_ref())?;

    if spec
        .endpoint_spec
        .as_ref()
        .and_then(|e| e.mode.as_ref())
        .is_some_and(|m| *m == EndpointSpecModeEnum::DNSRR)
    {
        return Err(SkipReason::DnsRr);
    }

    if spec
        .update_config
        .as_ref()
        .and_then(|u| u.order.as_ref())
        .is_some_and(|o| *o == ServiceSpecUpdateConfigOrderEnum::START_FIRST)
    {
        return Err(SkipReason::StartFirst);
    }

    let running = running_tasks.get(service_id).copied().unwrap_or(0);
    if running > 1 {
        return Err(SkipReason::MultipleRunningTasks { running });
    }

    let vip = service_vip(service, overlay_id).ok_or(SkipReason::NotOnOverlay)?;

    let labels = spec.labels.clone().unwrap_or_default();
    let port = labels
        .get("infrarust.port")
        .and_then(|p| p.trim().parse::<u16>().ok())
        .unwrap_or(DEFAULT_MC_PORT);
    let address = SocketAddr::new(vip, port).to_string();

    let mut config = service_labels_to_server_config(service_name, &labels, &address);
    if let Some(endpoint) = docker_endpoint
        && labels
            .get("infrarust.swarm.scale")
            .is_some_and(|v| v.trim() == "true")
    {
        let idle_timeout = Some(
            duration_label(&labels, "infrarust.swarm.idle_timeout")
                .unwrap_or(std::time::Duration::from_secs(300)),
        );
        let min_uptime = duration_label(&labels, "infrarust.swarm.min_uptime");
        let cooldown = duration_label(&labels, "infrarust.swarm.cooldown");
        let start_timeout = duration_label(&labels, "infrarust.swarm.wake_timeout")
            .unwrap_or(std::time::Duration::from_secs(60));
        let shutdown_timeout = duration_label(&labels, "infrarust.swarm.shutdown_timeout")
            .unwrap_or(std::time::Duration::from_secs(30));
        let readiness_timeout = duration_label(&labels, "infrarust.swarm.readiness_timeout")
            .unwrap_or(std::time::Duration::from_secs(3));
        let min_replicas = number_label(&labels, "infrarust.swarm.min_replicas", 0, 1);
        let max_replicas = number_label(&labels, "infrarust.swarm.max_replicas", 1, 1);
        config.server_manager = Some(ServerManagerConfig::DockerSwarm(DockerSwarmManagerConfig {
            endpoint: endpoint.to_string(),
            service_id: service_id.to_string(),
            game_port: port,
            network_id: Some(overlay_id.to_string()),
            min_replicas,
            max_replicas,
            shutdown_after: idle_timeout,
            min_uptime,
            cooldown,
            start_timeout,
            shutdown_timeout,
            readiness_timeout,
            poll_interval,
        }));
    }
    match infrarust_config::validate_server_config(&config) {
        Ok(warnings) => {
            for warning in warnings {
                tracing::warn!(service = %service_name, "{warning}");
            }
        }
        Err(e) => return Err(SkipReason::InvalidConfig(e.to_string())),
    }

    Ok(ProviderConfig {
        id: ProviderId::docker(format!("{SWARM_KEY_PREFIX}{service_id}")),
        config,
    })
}

fn duration_label(labels: &HashMap<String, String>, key: &str) -> Option<std::time::Duration> {
    labels
        .get(key)
        .and_then(|value| match humantime::parse_duration(value.trim()) {
            Ok(duration) => Some(duration),
            Err(error) => {
                tracing::warn!(label = %key, %error, "ignoring invalid Swarm autoscale duration");
                None
            }
        })
}

fn number_label(labels: &HashMap<String, String>, key: &str, default: u8, max: u8) -> u8 {
    let Some(value) = labels.get(key) else {
        return default;
    };
    match value.trim().parse::<u8>() {
        Ok(number) if number <= max => number,
        _ => {
            tracing::warn!(label = %key, value = %value, "ignoring unsupported Swarm autoscale replica bound");
            default
        }
    }
}

/// Accepts exactly one replicated service with 0 or 1 replicas.
fn check_mode(mode: Option<&ServiceSpecMode>) -> Result<(), SkipReason> {
    let mode = mode.ok_or(SkipReason::UnknownMode)?;
    if mode.global.is_some() {
        return Err(SkipReason::Global);
    }
    if mode.replicated_job.is_some() || mode.global_job.is_some() {
        return Err(SkipReason::Job);
    }
    let replicas = mode
        .replicated
        .as_ref()
        .and_then(|r| r.replicas)
        .ok_or(SkipReason::UnknownReplicas)?;
    match replicas {
        0 | 1 => Ok(()),
        n if n > 1 => Err(SkipReason::MultipleReplicas { replicas: n }),
        _ => Err(SkipReason::UnknownReplicas),
    }
}

/// The service's VIP on the configured overlay, if it is attached to it.
fn service_vip(service: &Service, overlay_id: &str) -> Option<IpAddr> {
    service
        .endpoint
        .as_ref()?
        .virtual_ips
        .as_ref()?
        .iter()
        .find(|vip| vip.network_id.as_deref() == Some(overlay_id))?
        .addr
        .as_deref()?
        .split('/')
        .next()?
        .parse()
        .ok()
}
