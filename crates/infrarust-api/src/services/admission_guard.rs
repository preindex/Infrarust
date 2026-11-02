//! Plugin-provided admission policy checks for backend destinations.

use uuid::Uuid;

use crate::error::ServiceError;
use crate::event::BoxFuture;
use crate::types::ServerId;

/// A namespaced policy supplied to an admission guard.
pub type AdmissionPolicy = serde_json::Value;

/// Connection path that triggered this admission check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdmissionStage {
    InitialConnection,
    ServerSwitch,
    AfterHold,
    ForwardedConnection,
}

/// Whether the guard needs an authenticated identity or client-only routing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct AdmissionRequirements {
    pub authenticated_identity: bool,
    pub client_only: bool,
}

impl AdmissionRequirements {
    #[must_use]
    pub const fn new(authenticated_identity: bool, client_only: bool) -> Self {
        Self {
            authenticated_identity,
            client_only,
        }
    }
}

/// Checks whether an authenticated player may join the final backend destination.
pub trait AdmissionGuard: Send + Sync {
    fn requirements(&self) -> AdmissionRequirements {
        AdmissionRequirements {
            authenticated_identity: true,
            client_only: false,
        }
    }

    fn authorize(
        &self,
        request: AdmissionRequest,
    ) -> BoxFuture<'_, Result<AdmissionDecision, ServiceError>>;
}

/// Request containing the final (post-redirect) backend destination.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AdmissionRequest {
    pub username: String,
    /// Player UUID; verified by Mojang only when `identity_verified` is true.
    pub uuid: Uuid,
    pub identity_verified: bool,
    pub server: ServerId,
    pub policy: AdmissionPolicy,
    pub stage: AdmissionStage,
}

impl AdmissionRequest {
    #[must_use]
    pub fn new(
        username: impl Into<String>,
        uuid: Uuid,
        identity_verified: bool,
        server: ServerId,
        policy: AdmissionPolicy,
        stage: AdmissionStage,
    ) -> Self {
        Self {
            username: username.into(),
            uuid,
            identity_verified,
            server,
            policy,
            stage,
        }
    }
}

/// Result of a guard check. `Hold` is not an admission; core re-runs every
/// configured guard after the named limbo handler completes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdmissionDecision {
    Allow,
    Deny { reason: Option<String> },
    Hold { handler: String },
}
