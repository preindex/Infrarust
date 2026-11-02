//! Generic admission checks for plugin-owned policies.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use infrarust_api::limbo::context::LimboEntryContext;
use infrarust_api::player::Player;
use infrarust_api::services::{
    AdmissionDecision, AdmissionGuard, AdmissionRequest, AdmissionStage, ProviderKind,
    ProviderRejected,
};
use infrarust_api::types::{Component, ServerId};
use infrarust_config::ServerConfig;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::player::PlayerSession;
use crate::routing::DomainRouter;
use crate::util::sync::{read, write};

const TIMEOUT: Duration = Duration::from_secs(5);
const UNAVAILABLE: &str = "Admission verification is unavailable right now.";
const UNVERIFIED: &str = "This server requires a verified Minecraft account.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionRegistration(u64);

struct GuardSlot {
    generation: AdmissionRegistration,
    guard: Arc<dyn AdmissionGuard>,
}

#[must_use]
pub struct Admission {
    server: ServerId,
    config: Arc<ServerConfig>,
    guards: Vec<(String, AdmissionRegistration)>,
}

pub enum Authorization {
    Admitted(Admission),
    Hold {
        namespace: String,
        handler: String,
        authenticated_uuid: Uuid,
    },
}

#[derive(Default)]
pub struct AdmissionService {
    guards: RwLock<HashMap<String, GuardSlot>>,
    generation: AtomicU64,
}

impl AdmissionService {
    pub fn register(
        &self,
        owner: &str,
        guard: Arc<dyn AdmissionGuard>,
    ) -> Result<AdmissionRegistration, ProviderRejected> {
        let mut guards = write(&self.guards);
        if guards.contains_key(owner) {
            return Err(ProviderRejected::AlreadyRegistered {
                kind: ProviderKind::AdmissionGuard,
            });
        }
        let registration =
            AdmissionRegistration(self.generation.fetch_add(1, Ordering::Relaxed) + 1);
        guards.insert(
            owner.to_owned(),
            GuardSlot {
                generation: registration,
                guard,
            },
        );
        tracing::info!(plugin = %owner, "admission guard registered");
        Ok(registration)
    }

    pub fn unregister(&self, owner: &str, registration: AdmissionRegistration) -> bool {
        let mut guards = write(&self.guards);
        if guards
            .get(owner)
            .is_some_and(|slot| slot.generation == registration)
        {
            guards.remove(owner);
            tracing::warn!(plugin = %owner, "admission guard unregistered");
            true
        } else {
            false
        }
    }

    pub fn report_missing(&self, routes: &DomainRouter) {
        let guards = read(&self.guards);
        for namespace in routes
            .list_all()
            .into_iter()
            .flat_map(|(_, config)| config.configured_admission_guards())
        {
            if !guards.contains_key(&namespace) {
                tracing::error!(
                    namespace = %namespace,
                    "configured admission guard is not registered; destinations requiring it will refuse players"
                );
            }
        }
    }

    pub async fn authorize(
        &self,
        session: &PlayerSession,
        server: &ServerId,
        config: &Arc<ServerConfig>,
        routes: &DomainRouter,
        cancel: &CancellationToken,
        stage: AdmissionStage,
    ) -> Result<Authorization, Component> {
        let profile = session.profile();
        let namespaces = config.configured_admission_guards();
        if namespaces.is_empty() {
            return Ok(Authorization::Admitted(Admission {
                server: server.clone(),
                config: Arc::clone(config),
                guards: Vec::new(),
            }));
        }
        let mut admitted_guards = Vec::new();
        for namespace in namespaces {
            let (generation, guard) = {
                let guards = read(&self.guards);
                let Some(slot) = guards.get(&namespace) else {
                    tracing::warn!(namespace = %namespace, server = %server, "required admission guard is missing");
                    return Err(Component::text(UNAVAILABLE));
                };
                (slot.generation, Arc::clone(&slot.guard))
            };
            let requirements = guard.requirements();
            if requirements.client_only
                && config.proxy_mode != infrarust_config::ProxyMode::ClientOnly
            {
                tracing::warn!(namespace = %namespace, server = %server, "admission guard requires client-only mode");
                return Err(Component::text(UNAVAILABLE));
            }
            let authenticated_uuid = session.authenticated_uuid();
            if requirements.authenticated_identity && authenticated_uuid.is_none() {
                return Err(Component::text(UNVERIFIED));
            }
            let uuid = authenticated_uuid.unwrap_or(profile.uuid);
            let request = AdmissionRequest::new(
                profile.username.clone(),
                uuid,
                authenticated_uuid.is_some(),
                server.clone(),
                config.admission_policy(&namespace),
                stage,
            );
            match self.ask(Arc::clone(&guard), request, cancel).await {
                Some(AdmissionDecision::Allow) => admitted_guards.push((namespace, generation)),
                Some(AdmissionDecision::Deny { reason }) => {
                    return Err(Component::text(
                        reason.unwrap_or_else(|| UNAVAILABLE.to_owned()),
                    ));
                }
                Some(AdmissionDecision::Hold { handler }) => {
                    let Some(authenticated_uuid) = authenticated_uuid else {
                        tracing::warn!(namespace = %namespace, server = %server, "admission guard requested a hold for an unverified identity");
                        return Err(Component::text(UNVERIFIED));
                    };
                    self.confirm_provider(&namespace, generation)?;
                    self.confirm_route(server, config, routes)?;
                    return Ok(Authorization::Hold {
                        namespace,
                        handler,
                        authenticated_uuid,
                    });
                }
                Some(_) => return Err(Component::text(UNAVAILABLE)),
                None => return Err(Component::text(UNAVAILABLE)),
            }
        }
        self.confirm_route(server, config, routes)?;
        for (namespace, generation) in &admitted_guards {
            self.confirm_provider(namespace, *generation)?;
        }
        Ok(Authorization::Admitted(Admission {
            server: server.clone(),
            config: Arc::clone(config),
            guards: admitted_guards,
        }))
    }

    pub fn confirm(&self, admission: &Admission, routes: &DomainRouter) -> Result<(), Component> {
        if admission.guards.is_empty() {
            return Ok(());
        }
        self.confirm_route(&admission.server, &admission.config, routes)?;
        if admission.config.configured_admission_guards().len() != admission.guards.len() {
            return Err(Component::text(UNAVAILABLE));
        }
        for (namespace, generation) in &admission.guards {
            self.confirm_provider(namespace, *generation)?;
        }
        Ok(())
    }

    pub fn hold_context(
        namespace: String,
        handler: String,
        server: ServerId,
        authenticated_uuid: Uuid,
    ) -> LimboEntryContext {
        LimboEntryContext::AdmissionGuard {
            namespace,
            handler,
            target_server: server,
            authenticated_uuid,
        }
    }

    fn confirm_route(
        &self,
        server: &ServerId,
        config: &Arc<ServerConfig>,
        routes: &DomainRouter,
    ) -> Result<(), Component> {
        if routes
            .find_route_by_server_id(server.as_str())
            .is_some_and(|(current, _)| Arc::ptr_eq(&current, config))
        {
            Ok(())
        } else {
            Err(Component::text(UNAVAILABLE))
        }
    }

    fn confirm_provider(
        &self,
        namespace: &str,
        generation: AdmissionRegistration,
    ) -> Result<(), Component> {
        if read(&self.guards)
            .get(namespace)
            .is_some_and(|slot| slot.generation == generation)
        {
            Ok(())
        } else {
            Err(Component::text(UNAVAILABLE))
        }
    }

    async fn ask(
        &self,
        guard: Arc<dyn AdmissionGuard>,
        request: AdmissionRequest,
        cancel: &CancellationToken,
    ) -> Option<AdmissionDecision> {
        let mut task = tokio::spawn(async move { guard.authorize(request).await });
        let abort = task.abort_handle();
        tokio::select! {
            () = cancel.cancelled() => { abort.abort(); None }
            joined = tokio::time::timeout(TIMEOUT, &mut task) => match joined {
                Ok(Ok(Ok(decision))) => Some(decision),
                Ok(Ok(Err(error))) => {
                    tracing::warn!(%error, "admission guard failed");
                    None
                }
                Ok(Err(error)) => {
                    tracing::warn!(%error, "admission guard panicked");
                    None
                }
                Err(_) => {
                    abort.abort();
                    tracing::warn!(timeout = ?TIMEOUT, "admission guard timed out");
                    None
                }
            }
        }
    }
}
