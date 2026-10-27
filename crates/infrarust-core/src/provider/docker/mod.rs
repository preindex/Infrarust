//! Docker-based configuration provider.
//!
//! Discovers Minecraft servers from Docker containers with `infrarust.*`
//! labels, or from Swarm services when `mode = "swarm"`, and watches Docker
//! events for real-time updates. Swarm mode also polls on `poll_interval`,
//! which is authoritative: a failed snapshot keeps the known routes.
//!
//! Entirely feature-gated behind `docker`.

pub mod labels;
pub mod swarm;

pub use labels::{
    labels_to_server_config, resolve_container_address, service_labels_to_server_config,
};

use std::collections::HashMap;
use std::pin::Pin;
use std::time::Duration;

use bollard::Docker;
use bollard::query_parameters::{
    EventsOptionsBuilder, InspectContainerOptions, InspectNetworkOptions,
    ListContainersOptionsBuilder, ListServicesOptionsBuilder, ListTasksOptionsBuilder,
};
use futures_util::StreamExt;
use tokio::sync::{Mutex, mpsc};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use infrarust_config::{DockerMode, DockerProviderConfig, ServerConfig};

use crate::error::CoreError;
use crate::provider::{ConfigProvider, ProviderConfig, ProviderEvent, ProviderId};
use swarm::{SkipReason, SkippedService, SwarmSnapshot};

/// Docker provider that auto-discovers containers or Swarm services.
pub struct DockerProvider {
    config: DockerProviderConfig,
    /// Known servers: unique id (container name, or `swarm:<service-id>`) → ServerConfig
    known: Mutex<HashMap<String, ServerConfig>>,
    /// Swarm services skipped on the last snapshot, so a skip is logged once
    /// per change instead of on every poll.
    skipped: Mutex<HashMap<String, SkipReason>>,
}

impl DockerProvider {
    pub fn new(config: &DockerProviderConfig) -> Self {
        Self {
            config: config.clone(),
            known: Mutex::new(HashMap::new()),
            skipped: Mutex::new(HashMap::new()),
        }
    }

    /// Connects to the Docker daemon.
    fn connect(&self) -> Result<Docker, CoreError> {
        let docker = if self.config.endpoint.starts_with("unix://") {
            Docker::connect_with_socket_defaults()
        } else if self.config.endpoint.starts_with("tcp://") {
            Docker::connect_with_http_defaults()
        } else {
            Docker::connect_with_local_defaults()
        }
        .map_err(|e| CoreError::DockerConnection(e.to_string()))?;

        Ok(docker)
    }

    /// Scans all running containers with `infrarust.enable=true`.
    async fn scan_containers(&self, docker: &Docker) -> Result<Vec<ProviderConfig>, CoreError> {
        let mut filters = HashMap::new();
        filters.insert("label", vec!["infrarust.enable=true"]);
        filters.insert("status", vec!["running"]);

        let options = ListContainersOptionsBuilder::default()
            .all(false)
            .filters(&filters)
            .build();

        let containers = docker
            .list_containers(Some(options))
            .await
            .map_err(|e| CoreError::DockerConnection(e.to_string()))?;

        let mut configs = Vec::new();

        for container in &containers {
            let container_id = match &container.id {
                Some(id) => id.as_str(),
                None => continue,
            };

            let container_name = container
                .names
                .as_ref()
                .and_then(|names| names.first())
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_else(|| container_id[..12.min(container_id.len())].to_string());

            match self
                .inspect_and_build(docker, container_id, &container_name)
                .await
            {
                Ok(Some(pc)) => configs.push(pc),
                Ok(None) => {} // Not enabled or no labels
                Err(e) => {
                    tracing::warn!(
                        container = %container_name,
                        error = %e,
                        "failed to process container, skipping"
                    );
                }
            }
        }

        Ok(configs)
    }

    /// Inspects a container and builds a ProviderConfig if it has infrarust labels.
    async fn inspect_and_build(
        &self,
        docker: &Docker,
        container_id: &str,
        container_name: &str,
    ) -> Result<Option<ProviderConfig>, CoreError> {
        let info = docker
            .inspect_container(container_id, None::<InspectContainerOptions>)
            .await
            .map_err(|e| CoreError::DockerConnection(e.to_string()))?;

        let labels = match info.config.as_ref().and_then(|c| c.labels.as_ref()) {
            Some(labels) => labels,
            None => return Ok(None),
        };

        if labels.get("infrarust.enable").map(|v| v.as_str()) != Some("true") {
            return Ok(None);
        }

        let port = labels
            .get("infrarust.port")
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(infrarust_config::DEFAULT_MC_PORT);

        let address = resolve_container_address(&info, self.config.network.as_deref(), port);

        let config = labels_to_server_config(container_name, labels, &address);

        let warnings = match infrarust_config::validate_server_config(&config) {
            Ok(warnings) => warnings,
            Err(e) => {
                tracing::warn!(
                    container = %container_name,
                    error = %e,
                    "skipping container with invalid config"
                );
                return Ok(None);
            }
        };
        for warning in warnings {
            tracing::warn!(container = %container_name, "{warning}");
        }

        Ok(Some(ProviderConfig {
            id: ProviderId::docker(container_name),
            config,
        }))
    }

    /// Watches Docker events with automatic reconnection.
    async fn watch_with_reconnect(
        &self,
        sender: &mpsc::Sender<ProviderEvent>,
        shutdown: &CancellationToken,
    ) -> Result<(), CoreError> {
        let mut reconnect_delay = self.config.reconnect_delay;
        let max_delay = Duration::from_secs(60);

        loop {
            let docker = match self.connect() {
                Ok(d) => d,
                Err(e) => {
                    if shutdown.is_cancelled() {
                        return Ok(());
                    }
                    tracing::warn!(
                        error = %e,
                        delay = ?reconnect_delay,
                        "failed to connect to docker, retrying"
                    );
                    tokio::select! {
                        biased;
                        () = shutdown.cancelled() => return Ok(()),
                        () = tokio::time::sleep(reconnect_delay) => {}
                    }
                    reconnect_delay = (reconnect_delay * 2).min(max_delay);
                    continue;
                }
            };

            // Reset delay on successful connection
            reconnect_delay = self.config.reconnect_delay;

            match self.watch_session(&docker, sender, shutdown).await {
                Ok(()) => return Ok(()), // Normal shutdown
                Err(e) => {
                    if shutdown.is_cancelled() {
                        return Ok(());
                    }
                    tracing::warn!(
                        error = %e,
                        delay = ?reconnect_delay,
                        "docker event stream disconnected, reconnecting"
                    );
                    tokio::select! {
                        biased;
                        () = shutdown.cancelled() => return Ok(()),
                        () = tokio::time::sleep(reconnect_delay) => {}
                    }
                    reconnect_delay = (reconnect_delay * 2).min(max_delay);

                    // Re-scan after reconnection
                    if let Ok(docker) = self.connect() {
                        self.resync(&docker, sender).await;
                    }
                }
            }
        }
    }

    /// Runs one connected watch session for the configured mode.
    async fn watch_session(
        &self,
        docker: &Docker,
        sender: &mpsc::Sender<ProviderEvent>,
        shutdown: &CancellationToken,
    ) -> Result<(), CoreError> {
        match self.config.mode {
            DockerMode::Container => self.watch_events(docker, sender, shutdown).await,
            DockerMode::Swarm => self.watch_swarm(docker, sender, shutdown).await,
        }
    }

    /// Watches the Docker event stream.
    async fn watch_events(
        &self,
        docker: &Docker,
        sender: &mpsc::Sender<ProviderEvent>,
        shutdown: &CancellationToken,
    ) -> Result<(), CoreError> {
        let mut filters = HashMap::new();
        filters.insert("type".to_string(), vec!["container".to_string()]);
        filters.insert(
            "event".to_string(),
            vec![
                "start".to_string(),
                "stop".to_string(),
                "die".to_string(),
                "destroy".to_string(),
                "pause".to_string(),
                "unpause".to_string(),
            ],
        );

        let options = EventsOptionsBuilder::default().filters(&filters).build();

        let mut stream = docker.events(Some(options));

        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    return Ok(());
                }
                event = stream.next() => {
                    match event {
                        Some(Ok(event)) => {
                            self.handle_docker_event(docker, &event, sender).await;
                        }
                        Some(Err(e)) => {
                            return Err(CoreError::DockerConnection(e.to_string()));
                        }
                        None => {
                            return Err(CoreError::DockerConnection("event stream ended".to_string()));
                        }
                    }
                }
            }
        }
    }

    /// Handles a single Docker event.
    async fn handle_docker_event(
        &self,
        docker: &Docker,
        event: &bollard::models::EventMessage,
        sender: &mpsc::Sender<ProviderEvent>,
    ) {
        let action = match &event.action {
            Some(a) => a.as_str(),
            None => return,
        };

        let container_id = match &event.actor {
            Some(actor) => match &actor.id {
                Some(id) => id.as_str(),
                None => return,
            },
            None => return,
        };

        let container_name = event
            .actor
            .as_ref()
            .and_then(|a| a.attributes.as_ref())
            .and_then(|attrs| attrs.get("name"))
            .cloned()
            .unwrap_or_else(|| container_id[..12.min(container_id.len())].to_string());

        match action {
            "start" | "unpause" => {
                match self
                    .inspect_and_build(docker, container_id, &container_name)
                    .await
                {
                    Ok(Some(pc)) => {
                        let mut known = self.known.lock().await;
                        let is_update = known.contains_key(&container_name);
                        known.insert(container_name.clone(), pc.config.clone());

                        let event = if is_update {
                            ProviderEvent::Updated(pc)
                        } else {
                            ProviderEvent::Added(pc)
                        };
                        let _ = sender.send(event).await;
                    }
                    Ok(None) => {} // Not an infrarust container
                    Err(e) => {
                        tracing::warn!(
                            container = %container_name,
                            error = %e,
                            "failed to inspect started container"
                        );
                    }
                }
            }
            "stop" | "die" | "pause" | "destroy" => {
                let mut known = self.known.lock().await;
                if known.remove(&container_name).is_some() {
                    let _ = sender
                        .send(ProviderEvent::Removed(ProviderId::docker(&container_name)))
                        .await;
                }
            }
            _ => {}
        }
    }

    /// Re-scans all containers and emits diffs against known state.
    async fn resync_containers(&self, docker: &Docker, sender: &mpsc::Sender<ProviderEvent>) {
        let current = match self.scan_containers(docker).await {
            Ok(configs) => configs,
            Err(e) => {
                tracing::warn!(error = %e, "failed to resync containers");
                return;
            }
        };

        let mut known = self.known.lock().await;

        let current_names: HashMap<String, ProviderConfig> = current
            .into_iter()
            .map(|pc| (pc.id.unique_id.clone(), pc))
            .collect();

        // Check for new / updated
        for (name, pc) in &current_names {
            if known.contains_key(name) {
                let _ = sender.send(ProviderEvent::Updated(pc.clone())).await;
            } else {
                let _ = sender.send(ProviderEvent::Added(pc.clone())).await;
            }
            known.insert(name.clone(), pc.config.clone());
        }

        // Check for removed
        let removed: Vec<String> = known
            .keys()
            .filter(|name| !current_names.contains_key(*name))
            .cloned()
            .collect();
        for name in removed {
            known.remove(&name);
            let _ = sender
                .send(ProviderEvent::Removed(ProviderId::docker(&name)))
                .await;
        }
    }
}

impl DockerProvider {
    /// Re-reads the configured mode's full state and emits diffs.
    async fn resync(&self, docker: &Docker, sender: &mpsc::Sender<ProviderEvent>) {
        match self.config.mode {
            DockerMode::Container => self.resync_containers(docker, sender).await,
            DockerMode::Swarm => {
                if let Err(e) = self.resync_swarm(docker, sender).await {
                    // Keep the known routes: a failed snapshot proves nothing.
                    tracing::warn!(error = %e, "failed to resync swarm services, keeping known routes");
                }
            }
        }
    }

    /// Takes one complete swarm snapshot from the manager API.
    ///
    /// Any API error fails the whole snapshot, so callers never see a partial
    /// listing that would remove routes.
    async fn swarm_snapshot(&self, docker: &Docker) -> Result<SwarmSnapshot, CoreError> {
        let docker_err = |e: bollard::errors::Error| CoreError::DockerConnection(e.to_string());

        let overlay_name = self.config.network.as_deref().ok_or_else(|| {
            CoreError::DockerConnection("docker.network is required in swarm mode".to_string())
        })?;
        let overlay = docker
            .inspect_network(overlay_name, None::<InspectNetworkOptions>)
            .await
            .map_err(docker_err)?;
        if overlay.driver.as_deref() != Some("overlay") {
            return Err(CoreError::DockerConnection(format!(
                "docker.network '{overlay_name}' is not an overlay network"
            )));
        }
        let overlay_id = overlay.id.ok_or_else(|| {
            CoreError::DockerConnection(format!("overlay '{overlay_name}' has no id"))
        })?;

        let mut service_filters = HashMap::new();
        service_filters.insert("label", vec![swarm::ENABLE_LABEL]);
        let services = docker
            .list_services(Some(
                ListServicesOptionsBuilder::default()
                    .filters(&service_filters)
                    .build(),
            ))
            .await
            .map_err(docker_err)?;

        // Tasks are only needed for the labelled services; skip the call when
        // there are none.
        let service_ids: Vec<&str> = services.iter().filter_map(|s| s.id.as_deref()).collect();
        let running = if service_ids.is_empty() {
            HashMap::new()
        } else {
            let mut task_filters = HashMap::new();
            task_filters.insert("service", service_ids);
            task_filters.insert("desired-state", vec!["running"]);
            let tasks = docker
                .list_tasks(Some(
                    ListTasksOptionsBuilder::default()
                        .filters(&task_filters)
                        .build(),
                ))
                .await
                .map_err(docker_err)?;
            swarm::count_running_tasks(&tasks)
        };

        Ok(swarm::build_snapshot_with_endpoint(
            &services,
            &running,
            &overlay_id,
            Some(&self.config.endpoint),
            self.config.poll_interval,
        ))
    }

    /// Logs skipped services, once per change, and records the new skip set.
    async fn report_skips(&self, skipped: &[SkippedService]) {
        let mut previous = self.skipped.lock().await;
        for s in skipped {
            if previous.get(&s.service_id) != Some(&s.reason) {
                tracing::warn!(
                    service = %s.service_name,
                    reason = %s.reason,
                    "skipping swarm service"
                );
            }
        }
        *previous = skipped
            .iter()
            .map(|s| (s.service_id.clone(), s.reason.clone()))
            .collect();
    }

    /// Polls the swarm snapshot and emits diffs against known routes.
    ///
    /// Only changed configs produce `Updated`, so a steady poll emits nothing.
    async fn resync_swarm(
        &self,
        docker: &Docker,
        sender: &mpsc::Sender<ProviderEvent>,
    ) -> Result<(), CoreError> {
        let snapshot = self.swarm_snapshot(docker).await?;
        self.report_skips(&snapshot.skipped).await;

        let mut known = self.known.lock().await;
        for event in swarm::diff_routes(&mut known, snapshot.routes) {
            let _ = sender.send(event).await;
        }

        Ok(())
    }

    /// Watches swarm service events and polls on `poll_interval`.
    ///
    /// The poll is authoritative. Events only trigger an earlier snapshot.
    async fn watch_swarm(
        &self,
        docker: &Docker,
        sender: &mpsc::Sender<ProviderEvent>,
        shutdown: &CancellationToken,
    ) -> Result<(), CoreError> {
        let mut filters = HashMap::new();
        filters.insert("type".to_string(), vec!["service".to_string()]);
        filters.insert(
            "event".to_string(),
            vec![
                "create".to_string(),
                "update".to_string(),
                "remove".to_string(),
            ],
        );
        let options = EventsOptionsBuilder::default().filters(&filters).build();
        let mut events = docker.events(Some(options));

        let mut ticker =
            tokio::time::interval(self.config.poll_interval.max(Duration::from_secs(1)));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first tick completes immediately; the caller has just synced.
        ticker.tick().await;

        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => return Ok(()),
                _ = ticker.tick() => {}
                event = events.next() => match event {
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(CoreError::DockerConnection(e.to_string())),
                    None => {
                        return Err(CoreError::DockerConnection("event stream ended".to_string()));
                    }
                },
            }
            self.resync(docker, sender).await;
        }
    }
}

impl ConfigProvider for DockerProvider {
    fn provider_type(&self) -> &str {
        "docker"
    }

    fn load_initial(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ProviderConfig>, CoreError>> + Send + '_>> {
        Box::pin(async move {
            let docker = self.connect()?;
            let configs = match self.config.mode {
                DockerMode::Container => self.scan_containers(&docker).await?,
                DockerMode::Swarm => {
                    let snapshot = self.swarm_snapshot(&docker).await?;
                    self.report_skips(&snapshot.skipped).await;
                    snapshot.routes
                }
            };

            // Store in known map
            let mut known = self.known.lock().await;
            for pc in &configs {
                known.insert(pc.id.unique_id.clone(), pc.config.clone());
            }

            tracing::info!(
                count = configs.len(),
                "docker provider loaded initial configs"
            );
            Ok(configs)
        })
    }

    fn watch(
        &self,
        sender: mpsc::Sender<ProviderEvent>,
        shutdown: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<(), CoreError>> + Send + '_>> {
        Box::pin(async move { self.watch_with_reconnect(&sender, &shutdown).await })
    }
}
