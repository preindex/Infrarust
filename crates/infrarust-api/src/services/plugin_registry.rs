//! Plugin registry trait — read-only view of loaded plugins.

use crate::error::PluginError;
use crate::event::BoxFuture;
use crate::plugin::{PluginMetadata, PluginRuntimeStatus, PluginState};

pub mod private {
    pub trait Sealed {}
}

#[derive(Debug, Clone)]
pub struct PluginInfo {
    pub metadata: PluginMetadata,
    pub state: PluginState,
    pub runtime: Option<PluginRuntimeStatus>,
}

impl PluginInfo {
    pub fn id(&self) -> &str {
        &self.metadata.id
    }
}

/// Read-only view of all loaded plugins.
pub trait PluginRegistry: Send + Sync + private::Sealed {
    fn list_plugin_info(&self) -> Vec<PluginInfo>;
    fn plugin_info(&self, id: &str) -> Option<PluginInfo>;

    fn plugin_admin_data<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, Result<serde_json::Value, PluginError>> {
        let _ = id;
        Box::pin(async {
            Err(PluginError::Service(
                crate::error::ServiceError::OperationFailed(
                    "plugin admin data is unavailable".into(),
                ),
            ))
        })
    }

    fn plugin_admin_action<'a>(
        &'a self,
        id: &'a str,
        action: &'a str,
        input: serde_json::Value,
    ) -> BoxFuture<'a, Result<serde_json::Value, PluginError>> {
        let _ = (id, action, input);
        Box::pin(async {
            Err(PluginError::Service(
                crate::error::ServiceError::OperationFailed(
                    "plugin admin action is unavailable".into(),
                ),
            ))
        })
    }

    fn is_loaded(&self, id: &str) -> bool {
        self.plugin_info(id).is_some()
    }
}
