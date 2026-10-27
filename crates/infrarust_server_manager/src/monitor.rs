//! Per-server monitoring: adaptive polling and auto-shutdown.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::error::ServerManagerError;
use crate::provider::ProviderStatus;
use crate::service::{PlayerCounter, ServerManagerService};
use crate::state::ServerState;

const SLOW_POLL_FACTOR: u32 = 6;

/// Per-server monitoring task.
///
/// Polls the provider for status changes, notifies waiters on transitions,
/// and triggers auto-shutdown when no players or demand are present.
///
/// The task belongs to one registration (`instance`). The service cancels
/// `token` when the server is unregistered or replaced, and the task also exits
/// if the registration no longer matches, so it never writes to a removed server.
pub async fn monitor_server(
    service: Arc<ServerManagerService>,
    server_id: String,
    instance: u64,
    player_counter: Arc<dyn PlayerCounter>,
    token: CancellationToken,
) {
    tracing::info!(server = %server_id, "monitoring task started");

    let mut fast_poll = false;

    loop {
        let interval = if fast_poll {
            service.get_poll_interval(&server_id)
        } else {
            service.get_poll_interval(&server_id) * SLOW_POLL_FACTOR
        };

        tokio::select! {
            biased;
            () = token.cancelled() => {
                tracing::info!(server = %server_id, "monitoring task shutting down");
                break;
            }
            () = tokio::time::sleep(interval) => {}
        }

        // Capture the registration and generation before polling so a transition
        // during the poll invalidates the result. A removed or replaced
        // registration ends this task.
        let Some(observed) = service
            .observe(&server_id)
            .filter(|observed| observed.instance == instance)
        else {
            break;
        };

        // Skip polling if server is Crashed (wait for ensure_started)
        if observed.state == ServerState::Crashed {
            continue;
        }

        // 1. Poll provider status
        let new_status = match service.check_provider_status(&server_id).await {
            Ok(status) => status,
            Err(e) => {
                tracing::warn!(server = %server_id, "status check failed, state is Unknown: {e}");
                // Report Unknown rather than keeping a stale state. A failed
                // poll never implies the server is empty, so no shutdown runs.
                service.update_state(&server_id, ProviderStatus::Unknown, observed);
                continue;
            }
        };

        // 2. Update state and detect transitions (drops the result if stale)
        if let Some((_old, new_state)) = service.update_state(&server_id, new_status, observed) {
            match new_state {
                ServerState::Online => {
                    service.notify_waiters(&server_id, &Ok(()));
                    fast_poll = false;
                }
                ServerState::Crashed => {
                    service.notify_waiters(
                        &server_id,
                        &Err(ServerManagerError::ProcessExited {
                            server_id: server_id.clone(),
                            exit_code: None,
                        }),
                    );
                    fast_poll = false;
                }
                ServerState::Sleeping => {
                    service.notify_waiters(
                        &server_id,
                        &Err(ServerManagerError::WentToSleep {
                            server_id: server_id.clone(),
                        }),
                    );
                    fast_poll = false;
                }
                ServerState::Starting | ServerState::Stopping => {
                    fast_poll = true;
                }
                _ => {}
            }
        }

        // 3. Auto-shutdown. The service re-checks demand, state and the
        //    observation under its lock, so a connect that lands now either
        //    holds demand (and cancels the stop) or is refused as Stopping.
        if new_status == ProviderStatus::Running {
            let player_count = player_counter.count_by_server(&server_id);
            service
                .stop_if_idle(&server_id, observed, player_count)
                .await;
        }
    }
}
