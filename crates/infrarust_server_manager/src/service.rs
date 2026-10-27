use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use infrarust_config::ServerManagerConfig;

use crate::crafty::CraftyProvider;
use crate::error::ServerManagerError;
use crate::local::LocalProvider;
use crate::provider::{ProviderStatus, ServerProvider};
use crate::pterodactyl::PterodactylProvider;
use crate::state::ServerState;

/// Trait for counting players on a server.
///
/// Implemented by the connection registry in `infrarust-core` to avoid
/// a circular dependency.
pub trait PlayerCounter: Send + Sync {
    /// Returns the number of players connected to the given server.
    fn count_by_server(&self, server_id: &str) -> usize;
}

/// Callback type for server state change notifications.
pub type StateChangeCallback = Arc<dyn Fn(&str, ServerState, ServerState) + Send + Sync>;

/// Central orchestrator for server management.
///
/// Tracks the state of each managed server, provides wake-up with waiters,
/// adaptive polling, and auto-shutdown after idle.
///
/// Servers can be registered and unregistered at any time, including after
/// monitoring has started. Each registered server has at most one monitoring
/// task while monitoring is running.
pub struct ServerManagerService {
    pub(crate) entries: DashMap<String, ServerEntry>,
    /// One monitoring task per registered server, keyed by server ID.
    monitors: DashMap<String, MonitorHandle>,
    /// Set by [`Self::start_monitoring`]; new registrations spawn monitors from it.
    monitoring: Mutex<Option<MonitoringContext>>,
    listeners: std::sync::RwLock<Vec<(u64, StateChangeCallback)>>,
    next_listener_id: AtomicU64,
    next_instance: AtomicU64,
}

/// Per-server scaling policy, passed at registration.
///
/// Lifecycle knobs are independent: the idle timeout decides when demand is
/// absent long enough to scale down, `min_uptime` keeps a freshly online
/// server up for a while, and `cooldown` spaces scale actions apart.
///
/// The first release is limited to one replica: `max_replicas` must be `1`
/// and `min_replicas` must be `0` or `1`. Other values are rejected at
/// registration rather than silently accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecyclePolicy {
    /// Inactivity (no players, no demand) before scale-down. `None` disables it.
    pub idle_timeout: Option<Duration>,
    /// Minimum time a server stays Online before it may be scaled down.
    pub min_uptime: Duration,
    /// Minimum time after the last start or stop before another scale-down.
    pub cooldown: Duration,
    /// Replicas kept running. `1` means the server is never scaled down for idleness.
    pub min_replicas: u32,
    /// Maximum replicas. Must be `1` in this release.
    pub max_replicas: u32,
}

impl Default for LifecyclePolicy {
    fn default() -> Self {
        Self {
            idle_timeout: None,
            min_uptime: Duration::ZERO,
            cooldown: Duration::ZERO,
            min_replicas: 0,
            max_replicas: 1,
        }
    }
}

impl LifecyclePolicy {
    /// Checks the policy is inside what this release supports.
    ///
    /// # Errors
    ///
    /// Returns [`ServerManagerError::InvalidPolicy`] if `min_replicas` is not
    /// `0` or `1`, or `max_replicas` is not `1`.
    pub fn validate(&self, server_id: &str) -> Result<(), ServerManagerError> {
        let reason = if self.min_replicas > 1 {
            Some(format!(
                "min_replicas must be 0 or 1, got {}",
                self.min_replicas
            ))
        } else if self.max_replicas != 1 {
            Some(format!(
                "max_replicas must be 1 in this release, got {}",
                self.max_replicas
            ))
        } else {
            None
        };

        reason.map_or(Ok(()), |reason| {
            Err(ServerManagerError::InvalidPolicy {
                server_id: server_id.to_string(),
                reason,
            })
        })
    }
}

pub(crate) struct ServerEntry {
    /// Identity of this registration. Replacing or removing a server creates a
    /// new registration, so a monitor or write from the old one can be told apart.
    pub(crate) instance: u64,
    /// Current state.
    pub(crate) state: ServerState,
    /// Transition counter: bumped on every state write so an in-flight monitor
    /// poll can detect that a transition intervened and drop its stale result.
    pub(crate) generation: u64,
    /// The provider controlling this server.
    pub(crate) provider: Arc<dyn ServerProvider>,
    /// Scaling policy for this server.
    pub(crate) policy: LifecyclePolicy,
    /// When the server last entered `Online` (for `min_uptime`).
    pub(crate) online_since: Option<Instant>,
    /// When the last start or stop was issued (for `cooldown`).
    pub(crate) last_scale_action: Option<Instant>,
    /// Maximum time to wait for the server to become Online.
    pub(crate) start_timeout: Duration,
    /// Base polling interval.
    pub(crate) poll_interval: Duration,
    /// Timestamp of the last player seen connected (for auto-shutdown).
    pub(crate) last_player_seen: Option<Instant>,
    /// Waiters: connections waiting for the server to become Online.
    pub(crate) waiters: Vec<oneshot::Sender<Result<(), ServerManagerError>>>,
    /// Set by a start request until the provider reports it starting or running:
    /// before this deadline a `Stopped` poll means the panel has not caught up yet.
    pub(crate) unconfirmed_start_until: Option<Instant>,
    /// Demand held against this server (connects, switches, transfers).
    pub(crate) demand: Arc<DemandState>,
}

impl ServerEntry {
    fn new(
        instance: u64,
        provider: Arc<dyn ServerProvider>,
        policy: LifecyclePolicy,
        start_timeout: Duration,
        poll_interval: Duration,
    ) -> Self {
        Self {
            instance,
            state: ServerState::Sleeping,
            generation: 0,
            provider,
            policy,
            online_since: None,
            last_scale_action: None,
            start_timeout,
            poll_interval,
            last_player_seen: None,
            waiters: Vec::new(),
            unconfirmed_start_until: None,
            demand: Arc::new(DemandState::default()),
        }
    }

    /// Sole mutation point for `state`: keeps the generation in sync.
    pub(crate) fn set_state(&mut self, new_state: ServerState) {
        self.generation += 1;
        self.state = new_state;
        self.unconfirmed_start_until = None;
        if new_state == ServerState::Online {
            self.online_since = Some(Instant::now());
        }
    }

    pub(crate) fn begin_start(&mut self) {
        self.set_state(ServerState::Starting);
        self.unconfirmed_start_until = Some(Instant::now() + self.start_timeout);
        self.last_scale_action = Some(Instant::now());
    }

    /// Whether the policy allows this server to be scaled down right now.
    fn may_scale_down(&self) -> bool {
        if self.policy.min_replicas > 0 {
            return false;
        }
        let uptime_ok = self
            .online_since
            .is_none_or(|since| since.elapsed() >= self.policy.min_uptime);
        let cooldown_ok = self
            .last_scale_action
            .is_none_or(|at| at.elapsed() >= self.policy.cooldown);
        uptime_ok && cooldown_ok
    }

    fn is_unconfirmed_start(&self) -> bool {
        self.unconfirmed_start_until
            .is_some_and(|deadline| Instant::now() < deadline)
    }

    /// Start of the current idle period: the later of the last player seen and
    /// the last demand release. `None` means the idle clock has not started.
    fn idle_since(&self) -> Option<Instant> {
        match (self.last_player_seen, self.demand.last_released()) {
            (Some(seen), Some(released)) => Some(seen.max(released)),
            (seen, released) => seen.or(released),
        }
    }
}

/// Demand counter shared by an entry and its [`DemandLease`]s.
///
/// Kept separate from the entry so a lease can be released after its entry was
/// removed without touching the service map.
#[derive(Default)]
pub(crate) struct DemandState {
    active: AtomicUsize,
    last_released: Mutex<Option<Instant>>,
}

impl DemandState {
    fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    fn last_released(&self) -> Option<Instant> {
        *lock(&self.last_released)
    }

    fn release(&self) {
        // Record the release before decrementing so an idle check that sees
        // zero demand also sees when it was released.
        *lock(&self.last_released) = Some(Instant::now());
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Demand held against a managed server.
///
/// While a lease is held, the server is not auto-stopped. Dropping the lease
/// releases the demand and restarts the idle clock from the release instant,
/// so the gap between a connect finishing and its session being recorded
/// cannot be mistaken for idleness.
///
/// A lease does not start the server. Pair it with
/// [`ServerManagerService::ensure_started`] to wake a sleeping server.
pub struct DemandLease {
    server_id: String,
    demand: Arc<DemandState>,
}

impl DemandLease {
    fn new(server_id: &str, demand: &Arc<DemandState>) -> Self {
        demand.active.fetch_add(1, Ordering::AcqRel);
        Self {
            server_id: server_id.to_string(),
            demand: Arc::clone(demand),
        }
    }

    /// The server this lease keeps awake.
    pub fn server_id(&self) -> &str {
        &self.server_id
    }
}

impl Drop for DemandLease {
    fn drop(&mut self) {
        self.demand.release();
    }
}

/// Snapshot of a registered server, taken before a poll or stop decision.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Observation {
    pub(crate) instance: u64,
    pub(crate) generation: u64,
    pub(crate) state: ServerState,
}

struct MonitoringContext {
    player_counter: Arc<dyn PlayerCounter>,
    shutdown: CancellationToken,
}

struct MonitorHandle {
    token: CancellationToken,
    _task: JoinHandle<()>,
}

/// Callers hold the state for a short time and never across an await.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn removed_error(server_id: &str) -> ServerManagerError {
    ServerManagerError::ServerNotFound {
        server_id: server_id.to_string(),
    }
}

/// Fails waiters of a removed registration instead of letting them hang.
fn fail_waiters(server_id: &str, waiters: Vec<oneshot::Sender<Result<(), ServerManagerError>>>) {
    if waiters.is_empty() {
        return;
    }
    tracing::debug!(server = %server_id, count = waiters.len(), "failing waiters of removed server");
    for tx in waiters {
        let _ = tx.send(Err(removed_error(server_id)));
    }
}

impl ServerManagerService {
    /// Creates the service from server configs.
    ///
    /// For each `(server_id, ServerManagerConfig)`, creates the appropriate provider
    /// and registers it with default state `Sleeping`.
    #[allow(clippy::needless_pass_by_value)]
    pub fn new(
        server_configs: &[(String, ServerManagerConfig)],
        http_client: reqwest::Client,
    ) -> Self {
        let service = Self::empty();

        for (server_id, config) in server_configs {
            let (provider, shutdown_after, start_timeout, poll_interval): (
                Arc<dyn ServerProvider>,
                Option<Duration>,
                Duration,
                Duration,
            ) = match config {
                ServerManagerConfig::Local(cfg) => (
                    Arc::new(LocalProvider::new(cfg.clone())),
                    cfg.shutdown_after,
                    cfg.start_timeout,
                    cfg.poll_interval,
                ),
                ServerManagerConfig::Pterodactyl(cfg) => (
                    Arc::new(PterodactylProvider::new(cfg, http_client.clone())),
                    cfg.shutdown_after,
                    cfg.start_timeout,
                    cfg.poll_interval,
                ),
                ServerManagerConfig::Crafty(cfg) => (
                    Arc::new(CraftyProvider::new(cfg, http_client.clone())),
                    cfg.shutdown_after,
                    cfg.start_timeout,
                    cfg.poll_interval,
                ),
            };

            let policy = LifecyclePolicy {
                idle_timeout: shutdown_after,
                ..LifecyclePolicy::default()
            };
            service.insert_entry(
                server_id.clone(),
                provider,
                policy,
                start_timeout,
                poll_interval,
            );

            tracing::info!(server = %server_id, "registered managed server");
        }

        service
    }

    fn empty() -> Self {
        Self {
            entries: DashMap::new(),
            monitors: DashMap::new(),
            monitoring: Mutex::new(None),
            listeners: std::sync::RwLock::new(Vec::new()),
            next_listener_id: AtomicU64::new(1),
            next_instance: AtomicU64::new(1),
        }
    }

    fn insert_entry(
        &self,
        server_id: String,
        provider: Arc<dyn ServerProvider>,
        policy: LifecyclePolicy,
        start_timeout: Duration,
        poll_interval: Duration,
    ) {
        let instance = self.next_instance.fetch_add(1, Ordering::Relaxed);
        self.entries.insert(
            server_id,
            ServerEntry::new(instance, provider, policy, start_timeout, poll_interval),
        );
    }

    /// Registers a server with a custom provider and scaling policy.
    ///
    /// Safe to call after [`Self::start_monitoring`]: a monitoring task is
    /// spawned for the server right away. Registering an existing ID replaces
    /// it: the old monitor is cancelled and its waiters fail.
    ///
    /// # Errors
    ///
    /// Returns [`ServerManagerError::InvalidPolicy`] if the policy is outside
    /// what this release supports (see [`LifecyclePolicy::validate`]). Nothing
    /// is registered in that case.
    pub fn register_server(
        self: &Arc<Self>,
        server_id: String,
        provider: Arc<dyn ServerProvider>,
        policy: LifecyclePolicy,
        start_timeout: Duration,
        poll_interval: Duration,
    ) -> Result<(), ServerManagerError> {
        policy.validate(&server_id)?;

        self.teardown_monitor(&server_id);
        if let Some((_, old)) = self.entries.remove(&server_id) {
            fail_waiters(&server_id, old.waiters);
        }
        self.insert_entry(
            server_id.clone(),
            provider,
            policy,
            start_timeout,
            poll_interval,
        );
        self.ensure_monitor(&server_id);
        Ok(())
    }

    /// Removes a server and stops its monitoring task.
    ///
    /// Waiters for the server fail with [`ServerManagerError::ServerNotFound`].
    /// Returns `false` if the ID was not registered.
    ///
    /// Monitoring is cancelled before the entry is removed, so an in-flight
    /// poll cannot write into a removed server.
    pub fn unregister_server(&self, server_id: &str) -> bool {
        self.teardown_monitor(server_id);
        let Some((_, entry)) = self.entries.remove(server_id) else {
            return false;
        };
        fail_waiters(server_id, entry.waiters);
        tracing::info!(server = %server_id, "unregistered managed server");
        true
    }

    /// Returns the current state of a server.
    pub fn get_state(&self, server_id: &str) -> Option<ServerState> {
        self.entries.get(server_id).map(|e| e.state)
    }

    /// Lists all managed servers and their states.
    pub fn get_all_managed(&self) -> Vec<(String, ServerState)> {
        self.entries
            .iter()
            .map(|e| (e.key().clone(), e.state))
            .collect()
    }

    /// Acquires demand on a server, keeping it from being auto-stopped.
    ///
    /// Serialized with the idle-stop decision: once this returns a lease, the
    /// server cannot move to `Stopping` for idleness until the lease is dropped.
    /// Works in any state except `Stopping`, which is refused.
    ///
    /// Does not start the server. Use [`Self::ensure_started`] to wake it.
    ///
    /// # Errors
    ///
    /// Returns [`ServerManagerError::ServerNotFound`] if the ID is unknown, or
    /// [`ServerManagerError::InvalidState`] if the server is stopping.
    pub fn acquire_demand(&self, server_id: &str) -> Result<DemandLease, ServerManagerError> {
        let entry = self
            .entries
            .get(server_id)
            .ok_or_else(|| removed_error(server_id))?;

        if entry.state == ServerState::Stopping {
            return Err(ServerManagerError::InvalidState {
                server_id: server_id.to_string(),
                state: ServerState::Stopping,
                action: "connect".to_string(),
            });
        }

        Ok(DemandLease::new(server_id, &entry.demand))
    }

    /// Returns the number of live demand leases on a server.
    ///
    /// Returns `None` if the server is not registered.
    pub fn demand_count(&self, server_id: &str) -> Option<usize> {
        self.entries
            .get(server_id)
            .map(|entry| entry.demand.active())
    }

    /// Registers a callback that fires on every state transition.
    ///
    /// The callback receives `(server_id, old_state, new_state)`.
    pub fn add_on_state_change(&self, callback: StateChangeCallback) -> u64 {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        let mut listeners = self
            .listeners
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        listeners.push((id, callback));
        id
    }

    fn fire_state_change(&self, server_id: &str, old: ServerState, new: ServerState) {
        let snapshot = {
            let listeners = self
                .listeners
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            listeners.clone()
        };
        for (_, callback) in &snapshot {
            if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                callback(server_id, old, new);
            })) {
                tracing::error!(
                    server = %server_id,
                    "state change callback panicked: {e:?}"
                );
            }
        }
    }

    /// Performs an initial health check for all managed servers.
    ///
    /// Calls `provider.check_status()` on each server to determine its initial state.
    /// A failed check leaves the server `Unknown`: it is not inferred to be stopped.
    /// Called once at proxy startup.
    pub async fn initial_health_check(&self) {
        let server_ids: Vec<String> = self.entries.iter().map(|e| e.key().clone()).collect();

        for server_id in server_ids {
            let provider = {
                let Some(entry) = self.entries.get(&server_id) else {
                    continue;
                };
                Arc::clone(&entry.provider)
            };

            match provider.check_status().await {
                Ok(status) => {
                    let new_state = ServerState::from(status);
                    let old_state = {
                        let Some(mut entry) = self.entries.get_mut(&server_id) else {
                            continue;
                        };
                        let old = entry.state;
                        entry.set_state(new_state);
                        old
                    };
                    if old_state != new_state {
                        self.fire_state_change(&server_id, old_state, new_state);
                    }
                    tracing::info!(
                        server = %server_id,
                        state = %new_state,
                        "initial health check complete"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        server = %server_id,
                        error = %e,
                        "initial health check failed, state is Unknown"
                    );
                    if let Some(observed) = self.observe(&server_id) {
                        self.update_state(&server_id, ProviderStatus::Unknown, observed);
                    }
                }
            }
        }
    }

    /// Ensures the server is started and ready for connections.
    ///
    /// - `Online` → returns Ok immediately
    /// - `Sleeping`/`Crashed`/`Unknown` → triggers start, then waits for Online
    /// - `Starting` → waits for Online (joins existing waiters)
    /// - `Stopping` → returns an error
    ///
    /// The caller holds demand for the whole wait, so an idle stop cannot run
    /// while the start is in flight.
    ///
    /// # Errors
    ///
    /// Returns [`ServerManagerError::ServerNotFound`] if the server ID is unknown
    /// or is removed while waiting, [`ServerManagerError::InvalidState`] if the
    /// server is stopping, [`ServerManagerError::StartTimeout`] if the server
    /// doesn't start in time, or [`ServerManagerError::Provider`] if the provider fails.
    pub async fn ensure_started(&self, server_id: &str) -> Result<(), ServerManagerError> {
        let (rx, start_timeout, _demand) = {
            let mut entry = self
                .entries
                .get_mut(server_id)
                .ok_or_else(|| removed_error(server_id))?;

            match entry.state {
                ServerState::Online => return Ok(()),
                ServerState::Stopping => {
                    return Err(ServerManagerError::InvalidState {
                        server_id: server_id.to_string(),
                        state: ServerState::Stopping,
                        action: "connect".to_string(),
                    });
                }
                ServerState::Starting => {
                    let (tx, rx) = oneshot::channel();
                    entry.waiters.push(tx);
                    let demand = DemandLease::new(server_id, &entry.demand);
                    (rx, entry.start_timeout, demand)
                }
                ServerState::Sleeping | ServerState::Crashed | ServerState::Unknown => {
                    let provider = Arc::clone(&entry.provider);
                    let instance = entry.instance;
                    let old_state = entry.state;
                    entry.begin_start();
                    let start_timeout = entry.start_timeout;
                    let (tx, rx) = oneshot::channel();
                    entry.waiters.push(tx);
                    let demand = DemandLease::new(server_id, &entry.demand);
                    drop(entry);

                    self.fire_state_change(server_id, old_state, ServerState::Starting);

                    if let Err(e) = provider.start().await {
                        tracing::error!(server = %server_id, "provider start failed: {e}");
                        if self.write_entry(server_id, instance, |entry| {
                            entry.set_state(ServerState::Crashed);
                        }) {
                            self.fire_state_change(
                                server_id,
                                ServerState::Starting,
                                ServerState::Crashed,
                            );
                        }
                        self.notify_waiters(server_id, &Err(e.clone()));
                        return Err(e);
                    }

                    (rx, start_timeout, demand)
                }
            }
        };

        // Wait for the server to become Online (or timeout)
        match tokio::time::timeout(start_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                // Channel closed without a message — monitoring task died
                Err(ServerManagerError::Provider {
                    server_id: server_id.to_string(),
                    message: "waiter channel closed unexpectedly".to_string(),
                })
            }
            Err(_) => Err(ServerManagerError::StartTimeout {
                server_id: server_id.to_string(),
                timeout: start_timeout,
            }),
        }
    }

    /// Starts a server manually.
    ///
    /// # Errors
    ///
    /// Returns [`ServerManagerError::ServerNotFound`] if the server ID is unknown,
    /// [`ServerManagerError::InvalidState`] if the server is not in a startable state,
    /// or a provider-specific error if the start command fails.
    pub async fn start_server(&self, server_id: &str) -> Result<(), ServerManagerError> {
        let (provider, old_state) = {
            let mut entry = self
                .entries
                .get_mut(server_id)
                .ok_or_else(|| removed_error(server_id))?;

            if !entry.state.is_startable() {
                return Err(ServerManagerError::InvalidState {
                    server_id: server_id.to_string(),
                    state: entry.state,
                    action: "start".to_string(),
                });
            }

            let old = entry.state;
            entry.begin_start();
            (Arc::clone(&entry.provider), old)
        };

        self.fire_state_change(server_id, old_state, ServerState::Starting);
        provider.start().await
    }

    /// Stops a server manually.
    ///
    /// # Errors
    ///
    /// Returns [`ServerManagerError::ServerNotFound`] if the server ID is unknown,
    /// or a provider-specific error if the stop command fails.
    pub async fn stop_server(&self, server_id: &str) -> Result<(), ServerManagerError> {
        let (instance, provider, old_state) = {
            let mut entry = self
                .entries
                .get_mut(server_id)
                .ok_or_else(|| removed_error(server_id))?;

            let old = entry.state;
            entry.set_state(ServerState::Stopping);
            (entry.instance, Arc::clone(&entry.provider), old)
        };

        self.run_stop(server_id, instance, provider, old_state)
            .await
    }

    /// Stops a server that has been idle since its `shutdown_after` deadline.
    ///
    /// The idle check and the move to `Stopping` happen under one entry lock,
    /// so a demand lease taken before it cancels the stop, and a lease taken
    /// after it is refused with `InvalidState`. The stop is skipped unless the
    /// observation is still current.
    pub(crate) async fn stop_if_idle(
        &self,
        server_id: &str,
        observed: Observation,
        player_count: usize,
    ) {
        let (provider, old_state) = {
            let Some(mut entry) = self.entries.get_mut(server_id) else {
                return;
            };

            if entry.instance != observed.instance
                || entry.generation != observed.generation
                || entry.state != ServerState::Online
            {
                return;
            }

            if player_count > 0 || entry.demand.active() > 0 {
                entry.last_player_seen = Some(Instant::now());
                return;
            }

            let Some(shutdown_after) = entry.policy.idle_timeout else {
                return;
            };

            if !entry.may_scale_down() {
                return;
            }

            let Some(idle_since) = entry.idle_since() else {
                entry.last_player_seen = Some(Instant::now());
                return;
            };

            if idle_since.elapsed() < shutdown_after {
                return;
            }

            let old = entry.state;
            entry.set_state(ServerState::Stopping);
            (Arc::clone(&entry.provider), old)
        };

        tracing::info!(
            server = %server_id,
            "auto-shutdown: no players or pending demand for shutdown_after duration"
        );
        if let Err(e) = self
            .run_stop(server_id, observed.instance, provider, old_state)
            .await
        {
            tracing::warn!(server = %server_id, "auto-shutdown failed: {e}");
        }
    }

    /// Runs the provider stop for an entry already in `Stopping`.
    ///
    /// Writes only if the entry is still `instance`, so a removed or replaced
    /// server is not written back to.
    async fn run_stop(
        &self,
        server_id: &str,
        instance: u64,
        provider: Arc<dyn ServerProvider>,
        old_state: ServerState,
    ) -> Result<(), ServerManagerError> {
        self.fire_state_change(server_id, old_state, ServerState::Stopping);

        if let Err(e) = provider.stop().await {
            if self.write_entry(server_id, instance, |entry| entry.set_state(old_state)) {
                self.fire_state_change(server_id, ServerState::Stopping, old_state);
            }
            return Err(e);
        }

        if self.write_entry(server_id, instance, |entry| {
            entry.set_state(ServerState::Sleeping);
            entry.last_player_seen = None;
            entry.last_scale_action = Some(Instant::now());
        }) {
            self.fire_state_change(server_id, ServerState::Stopping, ServerState::Sleeping);
        }

        Ok(())
    }

    /// Applies `f` only if the entry is still registration `instance`.
    /// Returns whether it was applied.
    fn write_entry(
        &self,
        server_id: &str,
        instance: u64,
        f: impl FnOnce(&mut ServerEntry),
    ) -> bool {
        let Some(mut entry) = self.entries.get_mut(server_id) else {
            return false;
        };
        if entry.instance != instance {
            return false;
        }
        f(&mut entry);
        true
    }

    /// Starts monitoring tasks for all registered servers.
    ///
    /// Idempotent: the first call keeps its player counter and shutdown token.
    /// Each call ensures every registered server has a monitoring task. Servers
    /// registered later are monitored as soon as they are registered.
    pub fn start_monitoring(
        self: &Arc<Self>,
        player_counter: Arc<dyn PlayerCounter>,
        shutdown: CancellationToken,
    ) {
        {
            let mut monitoring = lock(&self.monitoring);
            if monitoring.is_none() {
                *monitoring = Some(MonitoringContext {
                    player_counter,
                    shutdown,
                });
            }
        }

        let server_ids: Vec<String> = self.entries.iter().map(|e| e.key().clone()).collect();
        for server_id in server_ids {
            self.ensure_monitor(&server_id);
        }
    }

    /// Spawns the monitoring task for a server if monitoring is running and none exists.
    fn ensure_monitor(self: &Arc<Self>, server_id: &str) {
        let (player_counter, shutdown) = {
            let monitoring = lock(&self.monitoring);
            let Some(context) = monitoring.as_ref() else {
                return;
            };
            (
                Arc::clone(&context.player_counter),
                context.shutdown.clone(),
            )
        };

        let Some(instance) = self.entries.get(server_id).map(|entry| entry.instance) else {
            return;
        };

        let Entry::Vacant(slot) = self.monitors.entry(server_id.to_string()) else {
            return;
        };

        let token = shutdown.child_token();
        let service = Arc::clone(self);
        let id = server_id.to_string();
        let task_token = token.clone();
        let task = tokio::spawn(async move {
            crate::monitor::monitor_server(service, id, instance, player_counter, task_token).await;
        });
        slot.insert(MonitorHandle { token, _task: task });
        tracing::debug!(server = %server_id, "monitoring task spawned");
    }

    /// Cancels and forgets the monitoring task for a server, if any.
    fn teardown_monitor(&self, server_id: &str) {
        if let Some((_, handle)) = self.monitors.remove(server_id) {
            handle.token.cancel();
        }
    }

    /// Checks provider status for a server (used by monitoring task).
    pub(crate) async fn check_provider_status(
        &self,
        server_id: &str,
    ) -> Result<ProviderStatus, ServerManagerError> {
        let provider = {
            let entry = self
                .entries
                .get(server_id)
                .ok_or_else(|| removed_error(server_id))?;
            Arc::clone(&entry.provider)
        };

        provider.check_status().await
    }

    /// Snapshot of the current registration, taken before a poll.
    pub(crate) fn observe(&self, server_id: &str) -> Option<Observation> {
        self.entries.get(server_id).map(|entry| Observation {
            instance: entry.instance,
            generation: entry.generation,
            state: entry.state,
        })
    }

    /// Applies a polled status and returns the previous state if changed.
    ///
    /// `observed` must be captured **before** the poll; if a transition or a
    /// re-registration happened while the poll was in flight, the stale result
    /// is dropped so manual operations always win.
    pub(crate) fn update_state(
        &self,
        server_id: &str,
        new_status: ProviderStatus,
        observed: Observation,
    ) -> Option<(ServerState, ServerState)> {
        let (old_state, new_state) = {
            let mut entry = self.entries.get_mut(server_id)?;

            if entry.instance != observed.instance || entry.generation != observed.generation {
                tracing::debug!(
                    server = %server_id,
                    status = ?new_status,
                    "dropping stale poll result: a transition intervened during the poll"
                );
                return None;
            }

            match new_status {
                ProviderStatus::Starting | ProviderStatus::Running => {
                    entry.unconfirmed_start_until = None;
                }
                ProviderStatus::Stopped if entry.is_unconfirmed_start() => return None,
                _ => {}
            }

            let new_state = ServerState::from(new_status);
            let old_state = entry.state;

            if old_state == new_state {
                return None;
            }

            tracing::info!(
                server = %server_id,
                from = %old_state,
                to = %new_state,
                "state transition"
            );

            entry.set_state(new_state);
            drop(entry);
            (old_state, new_state)
        };

        // Fire callback outside DashMap lock
        self.fire_state_change(server_id, old_state, new_state);

        Some((old_state, new_state))
    }

    /// Notifies all waiters for a server with the given result.
    pub(crate) fn notify_waiters(&self, server_id: &str, result: &Result<(), ServerManagerError>) {
        if let Some(mut entry) = self.entries.get_mut(server_id) {
            let waiters = std::mem::take(&mut entry.waiters);
            drop(entry);

            let count = waiters.len();
            if count > 0 {
                tracing::debug!(server = %server_id, count, "notifying waiters");
            }
            for tx in waiters {
                let _ = tx.send(result.clone());
            }
        }
    }

    /// Returns the poll interval for a server.
    pub(crate) fn get_poll_interval(&self, server_id: &str) -> Duration {
        self.entries
            .get(server_id)
            .map_or_else(infrarust_config::defaults::poll_interval, |e| {
                e.poll_interval
            })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;

    use tokio::sync::Notify;

    use super::*;

    struct NoopProvider;

    impl ServerProvider for NoopProvider {
        fn start(&self) -> Pin<Box<dyn Future<Output = Result<(), ServerManagerError>> + Send>> {
            Box::pin(async { Ok(()) })
        }

        fn stop(&self) -> Pin<Box<dyn Future<Output = Result<(), ServerManagerError>> + Send>> {
            Box::pin(async { Ok(()) })
        }

        fn check_status(
            &self,
        ) -> Pin<Box<dyn Future<Output = Result<ProviderStatus, ServerManagerError>> + Send>>
        {
            Box::pin(async { Ok(ProviderStatus::Running) })
        }

        fn provider_type(&self) -> &'static str {
            "noop"
        }
    }

    /// Scriptable provider: fixed status or error, counted stops, optional stop gate.
    struct TestProvider {
        status: Result<ProviderStatus, ServerManagerError>,
        stops: AtomicUsize,
        stop_gate: Option<Arc<Notify>>,
    }

    impl TestProvider {
        fn new(status: Result<ProviderStatus, ServerManagerError>) -> Arc<Self> {
            Arc::new(Self {
                status,
                stops: AtomicUsize::new(0),
                stop_gate: None,
            })
        }

        fn gated(status: ProviderStatus, gate: Arc<Notify>) -> Arc<Self> {
            Arc::new(Self {
                status: Ok(status),
                stops: AtomicUsize::new(0),
                stop_gate: Some(gate),
            })
        }

        fn stop_count(&self) -> usize {
            self.stops.load(Ordering::SeqCst)
        }
    }

    impl ServerProvider for TestProvider {
        fn start(
            &self,
        ) -> Pin<Box<dyn Future<Output = Result<(), ServerManagerError>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }

        fn stop(
            &self,
        ) -> Pin<Box<dyn Future<Output = Result<(), ServerManagerError>> + Send + '_>> {
            Box::pin(async move {
                self.stops.fetch_add(1, Ordering::SeqCst);
                if let Some(gate) = &self.stop_gate {
                    gate.notified().await;
                }
                Ok(())
            })
        }

        fn check_status(
            &self,
        ) -> Pin<Box<dyn Future<Output = Result<ProviderStatus, ServerManagerError>> + Send + '_>>
        {
            Box::pin(async move { self.status.clone() })
        }

        fn provider_type(&self) -> &'static str {
            "test"
        }
    }

    struct NoPlayers;

    impl PlayerCounter for NoPlayers {
        fn count_by_server(&self, _server_id: &str) -> usize {
            0
        }
    }

    fn service_with_online_server(server_id: &str) -> Arc<ServerManagerService> {
        let service = Arc::new(ServerManagerService::new(&[], reqwest::Client::new()));
        service
            .register_server(
                server_id.to_string(),
                Arc::new(NoopProvider),
                LifecyclePolicy::default(),
                Duration::from_secs(5),
                Duration::from_secs(1),
            )
            .unwrap();
        let observed = service.observe(server_id).unwrap();
        service.update_state(server_id, ProviderStatus::Running, observed);
        assert_eq!(service.get_state(server_id), Some(ServerState::Online));
        service
    }

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        for _ in 0..400 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for condition");
    }

    /// A poll captured before `stop_server` must not resurrect the server,
    /// even when the manual op fully completes (Stopping → Sleeping) before
    /// the stale result lands.
    #[tokio::test]
    async fn stale_poll_does_not_resurrect_manually_stopped_server() {
        let service = service_with_online_server("s");

        let observed = service.observe("s").unwrap();
        service.stop_server("s").await.unwrap();
        assert_eq!(service.get_state("s"), Some(ServerState::Sleeping));

        let applied = service.update_state("s", ProviderStatus::Running, observed);
        assert_eq!(applied, None);
        assert_eq!(service.get_state("s"), Some(ServerState::Sleeping));
    }

    #[tokio::test]
    async fn online_to_sleeping_on_clean_stop_poll() {
        let service = service_with_online_server("s");

        let observed = service.observe("s").unwrap();
        let applied = service.update_state("s", ProviderStatus::Stopped, observed);
        assert_eq!(applied, Some((ServerState::Online, ServerState::Sleeping)));
    }

    #[tokio::test]
    async fn online_to_crashed_on_unexpected_exit_poll() {
        let service = service_with_online_server("s");

        let observed = service.observe("s").unwrap();
        let applied = service.update_state("s", ProviderStatus::Crashed, observed);
        assert_eq!(applied, Some((ServerState::Online, ServerState::Crashed)));
    }

    #[tokio::test]
    async fn stopped_poll_during_unconfirmed_start_keeps_waiting() {
        let service = service_with_online_server("s");
        service.stop_server("s").await.unwrap();
        service.start_server("s").await.unwrap();

        let observed = service.observe("s").unwrap();
        assert_eq!(
            service.update_state("s", ProviderStatus::Stopped, observed),
            None
        );
        assert_eq!(service.get_state("s"), Some(ServerState::Starting));

        assert_eq!(
            service.update_state("s", ProviderStatus::Starting, observed),
            None
        );
        assert_eq!(
            service.update_state("s", ProviderStatus::Stopped, observed),
            Some((ServerState::Starting, ServerState::Sleeping))
        );
    }

    #[tokio::test]
    async fn stopped_poll_after_start_deadline_goes_to_sleep() {
        let service = service_with_online_server("s");
        service.stop_server("s").await.unwrap();
        service.start_server("s").await.unwrap();
        service
            .entries
            .get_mut("s")
            .unwrap()
            .unconfirmed_start_until = Some(Instant::now());

        let observed = service.observe("s").unwrap();
        assert_eq!(
            service.update_state("s", ProviderStatus::Stopped, observed),
            Some((ServerState::Starting, ServerState::Sleeping))
        );
    }

    #[tokio::test]
    async fn register_after_monitoring_started_spawns_exactly_one_monitor() {
        let service = Arc::new(ServerManagerService::new(&[], reqwest::Client::new()));
        let shutdown = CancellationToken::new();
        service.start_monitoring(Arc::new(NoPlayers), shutdown.clone());
        assert!(service.monitors.is_empty());

        service
            .register_server(
                "late".to_string(),
                TestProvider::new(Ok(ProviderStatus::Running)),
                LifecyclePolicy::default(),
                Duration::from_secs(5),
                Duration::from_millis(5),
            )
            .unwrap();
        assert_eq!(service.monitors.len(), 1);

        // Re-registering replaces the monitor instead of adding a second one.
        service
            .register_server(
                "late".to_string(),
                TestProvider::new(Ok(ProviderStatus::Running)),
                LifecyclePolicy::default(),
                Duration::from_secs(5),
                Duration::from_millis(5),
            )
            .unwrap();
        assert_eq!(service.monitors.len(), 1);

        let polled = Arc::clone(&service);
        wait_until(move || polled.get_state("late") == Some(ServerState::Online)).await;

        shutdown.cancel();
    }

    #[tokio::test]
    async fn unregister_cancels_monitor_and_fails_waiter() {
        let service = Arc::new(ServerManagerService::new(&[], reqwest::Client::new()));
        service.start_monitoring(Arc::new(NoPlayers), CancellationToken::new());
        service
            .register_server(
                "s".to_string(),
                TestProvider::new(Ok(ProviderStatus::Starting)),
                LifecyclePolicy::default(),
                Duration::from_secs(30),
                Duration::from_millis(5),
            )
            .unwrap();

        let waiting = Arc::clone(&service);
        let waiter = tokio::spawn(async move { waiting.ensure_started("s").await });
        let observed = Arc::clone(&service);
        wait_until(move || observed.get_state("s") == Some(ServerState::Starting)).await;

        assert!(service.unregister_server("s"));

        let result = waiter.await.unwrap();
        assert!(matches!(
            result,
            Err(ServerManagerError::ServerNotFound { .. })
        ));
        assert!(service.monitors.is_empty());
        assert_eq!(service.get_state("s"), None);
        assert!(!service.unregister_server("s"));
    }

    #[tokio::test]
    async fn observation_from_replaced_registration_is_rejected() {
        let service = service_with_online_server("s");
        let old = service.observe("s").unwrap();

        service.unregister_server("s");
        service
            .register_server(
                "s".to_string(),
                Arc::new(NoopProvider),
                LifecyclePolicy::default(),
                Duration::from_secs(5),
                Duration::from_secs(1),
            )
            .unwrap();

        // A new registration starts at generation 0, the same value as a fresh
        // observation of the old one; only the instance tells them apart.
        let current = service.observe("s").unwrap();
        assert_ne!(old.instance, current.instance);
        assert_eq!(
            service.update_state("s", ProviderStatus::Running, old),
            None
        );
        assert_eq!(service.get_state("s"), Some(ServerState::Sleeping));
    }

    #[tokio::test]
    async fn held_demand_blocks_idle_stop() {
        let service = service_with_online_server("s");
        let provider = TestProvider::new(Ok(ProviderStatus::Running));
        service
            .register_server(
                "idle".to_string(),
                Arc::clone(&provider) as Arc<dyn ServerProvider>,
                LifecyclePolicy {
                    idle_timeout: Some(Duration::ZERO),

                    ..LifecyclePolicy::default()
                },
                Duration::from_secs(5),
                Duration::from_secs(1),
            )
            .unwrap();
        let observed = service.observe("idle").unwrap();
        service.update_state("idle", ProviderStatus::Running, observed);

        let lease = service.acquire_demand("idle").unwrap();
        assert_eq!(service.demand_count("idle"), Some(1));

        let observed = service.observe("idle").unwrap();
        service.stop_if_idle("idle", observed, 0).await;
        assert_eq!(service.get_state("idle"), Some(ServerState::Online));
        assert_eq!(provider.stop_count(), 0);

        drop(lease);
        assert_eq!(service.demand_count("idle"), Some(0));
    }

    #[tokio::test]
    async fn released_demand_restarts_the_idle_clock() {
        let service = Arc::new(ServerManagerService::new(&[], reqwest::Client::new()));
        let provider = TestProvider::new(Ok(ProviderStatus::Running));
        service
            .register_server(
                "s".to_string(),
                Arc::clone(&provider) as Arc<dyn ServerProvider>,
                LifecyclePolicy {
                    idle_timeout: Some(Duration::from_secs(3600)),

                    ..LifecyclePolicy::default()
                },
                Duration::from_secs(5),
                Duration::from_secs(1),
            )
            .unwrap();
        let observed = service.observe("s").unwrap();
        service.update_state("s", ProviderStatus::Running, observed);

        // The server has been idle for two hours by the monitor's clock...
        service.entries.get_mut("s").unwrap().last_player_seen =
            Some(Instant::now() - Duration::from_secs(7200));

        // ...but a connect just finished, so the idle clock restarts at release.
        drop(service.acquire_demand("s").unwrap());

        let observed = service.observe("s").unwrap();
        service.stop_if_idle("s", observed, 0).await;
        assert_eq!(service.get_state("s"), Some(ServerState::Online));
        assert_eq!(provider.stop_count(), 0);
    }

    #[tokio::test]
    async fn demand_is_refused_while_idle_stop_is_in_progress() {
        let service = Arc::new(ServerManagerService::new(&[], reqwest::Client::new()));
        let gate = Arc::new(Notify::new());
        let provider = TestProvider::gated(ProviderStatus::Running, Arc::clone(&gate));
        service
            .register_server(
                "s".to_string(),
                Arc::clone(&provider) as Arc<dyn ServerProvider>,
                LifecyclePolicy {
                    idle_timeout: Some(Duration::ZERO),

                    ..LifecyclePolicy::default()
                },
                Duration::from_secs(5),
                Duration::from_secs(1),
            )
            .unwrap();
        let observed = service.observe("s").unwrap();
        service.update_state("s", ProviderStatus::Running, observed);

        // The first idle check only starts the idle clock; the next one can stop.
        let observed = service.observe("s").unwrap();
        service.stop_if_idle("s", observed, 0).await;
        assert_eq!(service.get_state("s"), Some(ServerState::Online));

        let stopping = Arc::clone(&service);
        let observed = service.observe("s").unwrap();
        let stop = tokio::spawn(async move { stopping.stop_if_idle("s", observed, 0).await });

        let watched = Arc::clone(&service);
        wait_until(move || watched.get_state("s") == Some(ServerState::Stopping)).await;

        assert!(matches!(
            service.acquire_demand("s"),
            Err(ServerManagerError::InvalidState {
                state: ServerState::Stopping,
                ..
            })
        ));

        gate.notify_one();
        stop.await.unwrap();
        assert_eq!(service.get_state("s"), Some(ServerState::Sleeping));
        assert_eq!(provider.stop_count(), 1);
        assert!(service.acquire_demand("s").is_ok());
    }

    #[tokio::test]
    async fn api_failure_marks_server_unknown_instead_of_sleeping() {
        let service = Arc::new(ServerManagerService::new(&[], reqwest::Client::new()));
        service
            .register_server(
                "s".to_string(),
                TestProvider::new(Err(ServerManagerError::ApiResponse("down".to_string()))),
                LifecyclePolicy {
                    idle_timeout: Some(Duration::ZERO),

                    ..LifecyclePolicy::default()
                },
                Duration::from_secs(5),
                Duration::from_secs(1),
            )
            .unwrap();

        service.initial_health_check().await;
        assert_eq!(service.get_state("s"), Some(ServerState::Unknown));
    }

    #[tokio::test]
    async fn policy_rejects_unsupported_replicas_without_registering() {
        let service = Arc::new(ServerManagerService::new(&[], reqwest::Client::new()));

        let too_many = LifecyclePolicy {
            max_replicas: 2,
            ..LifecyclePolicy::default()
        };
        let result = service.register_server(
            "s".to_string(),
            TestProvider::new(Ok(ProviderStatus::Running)),
            too_many,
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        assert!(matches!(
            result,
            Err(ServerManagerError::InvalidPolicy { .. })
        ));

        let too_high_min = LifecyclePolicy {
            min_replicas: 2,
            ..LifecyclePolicy::default()
        };
        let result = service.register_server(
            "s".to_string(),
            TestProvider::new(Ok(ProviderStatus::Running)),
            too_high_min,
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        assert!(matches!(
            result,
            Err(ServerManagerError::InvalidPolicy { .. })
        ));

        assert_eq!(service.get_state("s"), None);
        assert!(service.monitors.is_empty());
    }

    #[tokio::test]
    async fn min_replicas_one_blocks_idle_stop() {
        let service = service_with_online_server("s");
        let provider = TestProvider::new(Ok(ProviderStatus::Running));
        let policy = LifecyclePolicy {
            idle_timeout: Some(Duration::ZERO),
            min_replicas: 1,
            ..LifecyclePolicy::default()
        };
        service
            .register_server(
                "pinned".to_string(),
                Arc::clone(&provider) as Arc<dyn ServerProvider>,
                policy,
                Duration::from_secs(5),
                Duration::from_secs(1),
            )
            .unwrap();
        let observed = service.observe("pinned").unwrap();
        service.update_state("pinned", ProviderStatus::Running, observed);

        let observed = service.observe("pinned").unwrap();
        service.stop_if_idle("pinned", observed, 0).await;
        assert_eq!(service.get_state("pinned"), Some(ServerState::Online));
        assert_eq!(provider.stop_count(), 0);
    }

    #[tokio::test]
    async fn cooldown_and_min_uptime_gate_scale_down() {
        let service = Arc::new(ServerManagerService::new(&[], reqwest::Client::new()));
        let provider = TestProvider::new(Ok(ProviderStatus::Running));
        let policy = LifecyclePolicy {
            idle_timeout: Some(Duration::ZERO),
            min_uptime: Duration::from_secs(3600),
            cooldown: Duration::from_secs(3600),
            ..LifecyclePolicy::default()
        };
        service
            .register_server(
                "s".to_string(),
                Arc::clone(&provider) as Arc<dyn ServerProvider>,
                policy,
                Duration::from_secs(5),
                Duration::from_secs(1),
            )
            .unwrap();
        let observed = service.observe("s").unwrap();
        service.update_state("s", ProviderStatus::Running, observed);

        // Freshly Online: min_uptime keeps it up.
        let observed = service.observe("s").unwrap();
        service.stop_if_idle("s", observed, 0).await;
        assert_eq!(provider.stop_count(), 0);

        // Uptime satisfied, but a scale action just happened: cooldown keeps it up.
        {
            let mut entry = service.entries.get_mut("s").unwrap();
            entry.online_since = Some(Instant::now() - Duration::from_secs(7200));
            entry.last_scale_action = Some(Instant::now());
        }
        let observed = service.observe("s").unwrap();
        service.stop_if_idle("s", observed, 0).await;
        assert_eq!(provider.stop_count(), 0);
        assert_eq!(service.get_state("s"), Some(ServerState::Online));
    }
}
