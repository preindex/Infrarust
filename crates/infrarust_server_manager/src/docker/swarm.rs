//! Swarm provider: scales one singleton replicated service between 0 and 1.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::time::{Duration, Instant};

use bollard::Docker;
use bollard::errors::Error as DockerError;
use bollard::models::{NodeState, Service, ServiceSpec, Task, TaskState};
use bollard::query_parameters::{
    InspectServiceOptions, ListTasksOptions, UpdateServiceOptionsBuilder,
};
use infrarust_config::DockerSwarmManagerConfig;

use super::ping::status_ping;
use crate::error::ServerManagerError;
use crate::provider::{ProviderStatus, ServerProvider};

/// Services must carry this label set to `true` to be scaled. Removing it, or
/// changing it, opts the service out: the provider stops touching it.
pub const SCALE_OPT_IN_LABEL: &str = "infrarust.swarm.scale";

/// Bounded re-read and revalidation attempts for out-of-sequence update conflicts.
const MAX_UPDATE_ATTEMPTS: u32 = 3;
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Provider for one Docker Swarm service, addressed by its immutable ID.
///
/// `start` and `stop` run under one lock, so concurrent calls are coalesced:
/// each waits for the previous one, then observes the replica count it left.
pub struct SwarmServerProvider {
    config: DockerSwarmManagerConfig,
    docker: Result<Docker, String>,
    lifecycle: tokio::sync::Mutex<()>,
}

impl SwarmServerProvider {
    pub fn new(config: &DockerSwarmManagerConfig) -> Self {
        Self {
            docker: connect(&config.endpoint),
            config: config.clone(),
            lifecycle: tokio::sync::Mutex::new(()),
        }
    }

    /// The configuration this provider was built from. Lifecycle policy such as
    /// `min_uptime` and `cooldown` is read from here.
    pub fn config(&self) -> &DockerSwarmManagerConfig {
        &self.config
    }

    fn provider_error(&self, message: impl Into<String>) -> ServerManagerError {
        ServerManagerError::Provider {
            server_id: self.config.service_id.clone(),
            message: message.into(),
        }
    }

    fn docker(&self) -> Result<&Docker, ServerManagerError> {
        self.docker
            .as_ref()
            .map_err(|e| self.provider_error(format!("docker client unavailable: {e}")))
    }

    async fn inspect(&self, docker: &Docker) -> Result<Service, ServerManagerError> {
        docker
            .inspect_service(&self.config.service_id, None::<InspectServiceOptions>)
            .await
            .map_err(|e| self.provider_error(format!("inspect service: {e}")))
    }

    async fn list_tasks(&self, docker: &Docker) -> Result<Vec<Task>, ServerManagerError> {
        let filters =
            HashMap::from([("service".to_string(), vec![self.config.service_id.clone()])]);
        docker
            .list_tasks(Some(ListTasksOptions {
                filters: Some(filters),
            }))
            .await
            .map_err(|e| self.provider_error(format!("list tasks: {e}")))
    }

    /// Checks that the service is still the one this provider controls and
    /// returns its replica count. Anything else is an error, never a scale target.
    fn validate(&self, service: &Service) -> Result<u8, String> {
        if service.id.as_deref() != Some(self.config.service_id.as_str()) {
            return Err("service ID does not match the configured ID".to_string());
        }
        let labels = service.spec.as_ref().and_then(|spec| spec.labels.as_ref());
        if labels
            .and_then(|l| l.get(SCALE_OPT_IN_LABEL))
            .map(String::as_str)
            != Some("true")
        {
            return Err(format!(
                "service opted out: label {SCALE_OPT_IN_LABEL} is not true"
            ));
        }
        let mode = service
            .spec
            .as_ref()
            .and_then(|spec| spec.mode.as_ref())
            .ok_or("service has no mode")?;
        if mode.global.is_some() || mode.global_job.is_some() || mode.replicated_job.is_some() {
            return Err("only replicated services are supported".to_string());
        }
        let replicas = mode
            .replicated
            .as_ref()
            .and_then(|replicated| replicated.replicas)
            .ok_or("service is not a replicated service")?;
        if !(0..=1).contains(&replicas) {
            return Err(format!(
                "service has {replicas} replicas; only singleton services (0 or 1) are supported"
            ));
        }
        u8::try_from(replicas).map_err(|_| "replica count out of range".to_string())
    }

    /// Scales the service to `target` (0 or 1), changing only `Mode.Replicated.Replicas`.
    ///
    /// Re-reads the service on every attempt, so the update carries the current
    /// version and is revalidated before each write. Out-of-sequence conflicts
    /// retry up to `MAX_UPDATE_ATTEMPTS` times; anything else fails immediately.
    async fn set_replicas(&self, docker: &Docker, target: u8) -> Result<(), ServerManagerError> {
        for attempt in 1..=MAX_UPDATE_ATTEMPTS {
            let service = self.inspect(docker).await?;
            let current = self
                .validate(&service)
                .map_err(|reason| self.provider_error(format!("refusing to scale: {reason}")))?;
            if current == target {
                return Ok(());
            }

            let version = service
                .version
                .as_ref()
                .and_then(|v| v.index)
                .ok_or_else(|| self.provider_error("service has no version index"))?;
            let version = i32::try_from(version)
                .map_err(|_| self.provider_error("service version out of range"))?;

            let spec = spec_with_replicas(service.spec.unwrap_or_default(), target);
            let options = UpdateServiceOptionsBuilder::new().version(version).build();
            match docker
                .update_service(&self.config.service_id, spec, options, None)
                .await
            {
                Ok(_) => {
                    tracing::info!(
                        service = %self.config.service_id,
                        replicas = target,
                        "updated swarm service replicas"
                    );
                    return Ok(());
                }
                Err(error) if is_out_of_sequence(&error) => {
                    if attempt == MAX_UPDATE_ATTEMPTS {
                        return Err(self.provider_error(format!(
                            "service kept changing; gave up after {MAX_UPDATE_ATTEMPTS} attempts: {error}"
                        )));
                    }
                    tracing::debug!(
                        service = %self.config.service_id,
                        attempt,
                        "swarm service changed under update, re-reading"
                    );
                }
                Err(error) => {
                    return Err(self.provider_error(format!("update service: {error}")));
                }
            }
        }
        unreachable!("the final attempt always returns")
    }

    /// Waits until every task of the service is terminal, bounded by `shutdown_timeout`.
    async fn wait_for_terminal_tasks(&self, docker: &Docker) -> Result<(), ServerManagerError> {
        let deadline = Instant::now() + self.config.shutdown_timeout;
        loop {
            let tasks = self.list_tasks(docker).await?;
            if tasks.iter().all(is_terminal) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(self.provider_error(format!(
                    "tasks still active after {:?}",
                    self.config.shutdown_timeout
                )));
            }
            tokio::time::sleep(STOP_POLL_INTERVAL).await;
        }
    }

    /// Address of the service VIP on the configured network. Tasks are never probed.
    fn vip_address(&self, service: &Service) -> Result<IpAddr, String> {
        let virtual_ips = service
            .endpoint
            .as_ref()
            .and_then(|endpoint| endpoint.virtual_ips.as_ref())
            .map_or(&[][..], Vec::as_slice);
        let entry = match (&self.config.network_id, virtual_ips) {
            (Some(network), ips) => ips
                .iter()
                .find(|ip| ip.network_id.as_deref() == Some(network.as_str())),
            (None, [only]) => Some(only),
            (None, []) => None,
            (None, _) => {
                return Err("service has several VIPs; set network_id to choose one".to_string());
            }
        };
        let addr = entry
            .and_then(|ip| ip.addr.as_deref())
            .ok_or("service has no VIP on the configured network")?;
        addr.split('/')
            .next()
            .and_then(|ip| ip.parse().ok())
            .ok_or_else(|| format!("unparseable VIP address {addr}"))
    }

    /// Running requires: exactly one desired task running, on a ready node,
    /// and a Minecraft status ping to the service VIP that succeeds.
    async fn running_status(
        &self,
        docker: &Docker,
        service: &Service,
    ) -> Result<ProviderStatus, ServerManagerError> {
        let tasks = self.list_tasks(docker).await?;
        let desired: Vec<&Task> = tasks
            .iter()
            .filter(|task| !is_terminal(task) && task.desired_state == Some(TaskState::RUNNING))
            .collect();
        let task = match desired.as_slice() {
            [] => return Ok(ProviderStatus::Starting),
            [only] => *only,
            _ => return Ok(ProviderStatus::Unknown),
        };
        if task_state(task) != Some(TaskState::RUNNING) {
            return Ok(ProviderStatus::Starting);
        }

        let Some(node_id) = task.node_id.as_deref() else {
            return Ok(ProviderStatus::Starting);
        };
        let node = docker
            .inspect_node(node_id)
            .await
            .map_err(|e| self.provider_error(format!("inspect node: {e}")))?;
        let node_ready =
            node.status.as_ref().and_then(|status| status.state) == Some(NodeState::READY);
        if !node_ready {
            return Ok(ProviderStatus::Starting);
        }

        let vip = self
            .vip_address(service)
            .map_err(|reason| self.provider_error(reason))?;
        let addr = SocketAddr::new(vip, self.config.game_port);
        match status_ping(addr, self.config.readiness_timeout).await {
            Ok(()) => Ok(ProviderStatus::Running),
            Err(reason) => {
                tracing::debug!(
                    service = %self.config.service_id,
                    %addr,
                    reason,
                    "readiness ping to service VIP failed"
                );
                Ok(ProviderStatus::Starting)
            }
        }
    }
}

fn connect(endpoint: &str) -> Result<Docker, String> {
    let client = if let Some(path) = endpoint.strip_prefix("unix://") {
        Docker::connect_with_socket(path, 120, bollard::API_DEFAULT_VERSION)
    } else {
        Docker::connect_with_http(endpoint, 120, bollard::API_DEFAULT_VERSION)
    };
    client.map_err(|e| format!("cannot connect to {endpoint}: {e}"))
}

fn task_state(task: &Task) -> Option<TaskState> {
    task.status.as_ref().and_then(|status| status.state)
}

/// Terminal tasks no longer hold a slot or a container. A task without status
/// is treated as live, so an unknown task keeps `stop` waiting.
fn is_terminal(task: &Task) -> bool {
    matches!(
        task_state(task),
        Some(
            TaskState::COMPLETE
                | TaskState::SHUTDOWN
                | TaskState::FAILED
                | TaskState::REJECTED
                | TaskState::ORPHANED
                | TaskState::REMOVE
        )
    )
}

/// Returns `spec` with `Mode.Replicated.Replicas` set and nothing else touched.
///
/// Every other field is passed through the typed model as read. The roundtrip
/// test below proves that nothing else changes in the serialized spec.
fn spec_with_replicas(mut spec: ServiceSpec, replicas: u8) -> ServiceSpec {
    spec.mode
        .get_or_insert_with(Default::default)
        .replicated
        .get_or_insert_with(Default::default)
        .replicas = Some(i64::from(replicas));
    spec
}

/// Docker answers a stale update with a server error that says "update out of sequence".
fn is_out_of_sequence(error: &DockerError) -> bool {
    match error {
        DockerError::DockerResponseServerError {
            status_code,
            message,
        } => *status_code == 409 || message.to_ascii_lowercase().contains("out of sequence"),
        _ => false,
    }
}

impl ServerProvider for SwarmServerProvider {
    fn start(&self) -> Pin<Box<dyn Future<Output = Result<(), ServerManagerError>> + Send + '_>> {
        Box::pin(async move {
            let _lifecycle = self.lifecycle.lock().await;
            let docker = self.docker()?;
            self.set_replicas(docker, 1).await
        })
    }

    fn stop(&self) -> Pin<Box<dyn Future<Output = Result<(), ServerManagerError>> + Send + '_>> {
        Box::pin(async move {
            let _lifecycle = self.lifecycle.lock().await;
            let docker = self.docker()?;
            self.set_replicas(docker, 0).await?;
            // The API update alone does not mean the container is gone.
            self.wait_for_terminal_tasks(docker).await
        })
    }

    fn check_status(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderStatus, ServerManagerError>> + Send + '_>> {
        Box::pin(async move {
            let docker = self.docker()?;
            let service = self.inspect(docker).await?;
            let replicas = self
                .validate(&service)
                .map_err(|reason| self.provider_error(format!("service not managed: {reason}")))?;

            if replicas == 0 {
                let tasks = self.list_tasks(docker).await?;
                return Ok(if tasks.iter().all(is_terminal) {
                    ProviderStatus::Stopped
                } else {
                    ProviderStatus::Stopping
                });
            }
            self.running_status(docker, &service).await
        })
    }

    fn provider_type(&self) -> &'static str {
        "docker_swarm"
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    const SERVICE_JSON: &str = include_str!("../../tests/fixtures/swarm_service.json");

    fn fixture() -> (Value, Service) {
        let raw: Value = serde_json::from_str(SERVICE_JSON).unwrap();
        let service: Service = serde_json::from_value(raw.clone()).unwrap();
        (raw, service)
    }

    /// Proof that the update body differs from the inspected spec in `Replicas` only.
    /// A typed field that the bollard model drops or renames fails this comparison.
    #[test]
    fn scaling_changes_only_the_replica_count() {
        let (raw, service) = fixture();
        for target in [0u8, 1] {
            let spec = spec_with_replicas(service.spec.clone().unwrap(), target);
            let mut expected = raw["Spec"].clone();
            expected["Mode"]["Replicated"]["Replicas"] = json!(target);
            assert_eq!(
                serde_json::to_value(&spec).unwrap(),
                expected,
                "update body must match the inspected spec except Replicas"
            );
        }
    }

    #[test]
    fn validation_accepts_a_singleton_opted_in_service() {
        let (_, service) = fixture();
        let provider = provider_for("e2ztjcpm0qgbqaiscyz2ngx1k");
        assert_eq!(provider.validate(&service), Ok(1));
    }

    #[test]
    fn validation_refuses_an_opted_out_service() {
        let (_, mut service) = fixture();
        service
            .spec
            .as_mut()
            .unwrap()
            .labels
            .as_mut()
            .unwrap()
            .insert(SCALE_OPT_IN_LABEL.to_string(), "false".to_string());
        let provider = provider_for("e2ztjcpm0qgbqaiscyz2ngx1k");
        assert!(
            provider
                .validate(&service)
                .unwrap_err()
                .contains("opted out")
        );
    }

    #[test]
    fn validation_refuses_more_than_one_replica() {
        let (_, mut service) = fixture();
        service.spec.as_mut().unwrap().mode = Some(bollard::models::ServiceSpecMode {
            replicated: Some(bollard::models::ServiceSpecModeReplicated { replicas: Some(3) }),
            ..Default::default()
        });
        let provider = provider_for("e2ztjcpm0qgbqaiscyz2ngx1k");
        assert!(
            provider
                .validate(&service)
                .unwrap_err()
                .contains("singleton")
        );
    }

    #[test]
    fn validation_refuses_global_mode() {
        let (_, mut service) = fixture();
        service.spec.as_mut().unwrap().mode = Some(bollard::models::ServiceSpecMode {
            global: Some(Default::default()),
            ..Default::default()
        });
        let provider = provider_for("e2ztjcpm0qgbqaiscyz2ngx1k");
        assert!(
            provider
                .validate(&service)
                .unwrap_err()
                .contains("replicated")
        );
    }

    #[test]
    fn validation_refuses_a_different_service_id() {
        let (_, service) = fixture();
        let provider = provider_for("someone-else");
        assert!(provider.validate(&service).unwrap_err().contains("ID"));
    }

    #[test]
    fn vip_is_taken_from_the_configured_network_not_a_task() {
        let (_, service) = fixture();
        let provider = provider_for("e2ztjcpm0qgbqaiscyz2ngx1k");
        assert_eq!(
            provider.vip_address(&service),
            Ok("10.0.1.2".parse().unwrap())
        );
    }

    #[test]
    fn several_vips_without_a_network_are_refused() {
        let (_, mut service) = fixture();
        let endpoint = service.endpoint.as_mut().unwrap();
        endpoint
            .virtual_ips
            .as_mut()
            .unwrap()
            .push(bollard::models::ServiceEndpointVirtualIps {
                network_id: Some("ingress".to_string()),
                addr: Some("10.255.0.9/16".to_string()),
            });
        let provider = provider_for("e2ztjcpm0qgbqaiscyz2ngx1k");
        assert!(
            provider
                .vip_address(&service)
                .unwrap_err()
                .contains("network_id")
        );
    }

    fn provider_for(service_id: &str) -> SwarmServerProvider {
        let config = DockerSwarmManagerConfig {
            endpoint: "tcp://127.0.0.1:1".to_string(),
            service_id: service_id.to_string(),
            game_port: 25565,
            network_id: None,
            min_replicas: 0,
            max_replicas: 1,
            shutdown_after: None,
            min_uptime: None,
            cooldown: None,
            start_timeout: Duration::from_secs(10),
            shutdown_timeout: Duration::from_secs(10),
            readiness_timeout: Duration::from_secs(1),
            poll_interval: Duration::from_secs(5),
        };
        SwarmServerProvider::new(&config)
    }
}
