use crate::types::{Component, ServerId};

/// Describes why a player entered the limbo world.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum LimboEntryContext {
    InitialConnection {
        target_server: ServerId,
    },
    /// A plugin admission guard requested a hold before the destination.
    AdmissionGuard {
        namespace: String,
        target_server: ServerId,
        authenticated_uuid: uuid::Uuid,
        handler: String,
    },
    KickedFromServer {
        server: ServerId,
        reason: Component,
    },
    PluginRedirect {
        from_server: Option<ServerId>,
    },
}
