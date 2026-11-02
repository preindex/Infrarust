use std::time::Duration;

use infrarust_api::plugin::{
    PluginAdminAction, PluginAdminColumn, PluginAdminComponent, PluginAdminField,
    PluginAdminFieldKind, PluginAdminPage, PluginAdminTone, PluginFault, PluginRuntimeStatus,
};
use infrarust_api::services::plugin_registry::PluginInfo;
use serde::Serialize;

#[derive(Serialize)]
pub struct PluginResponse {
    pub id: String,
    pub name: String,
    pub version: String,
    pub authors: Vec<String>,
    pub description: Option<String>,
    pub state: String,
    pub dependencies: Vec<PluginDependencyResponse>,
    pub runtime: Option<PluginRuntimeResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admin_page: Option<PluginAdminPageResponse>,
}

impl PluginResponse {
    pub fn from_info(info: PluginInfo) -> Self {
        let state = info.state.as_str().to_string();
        let runtime = info.runtime.map(PluginRuntimeResponse::from_status);
        let meta = info.metadata;
        let admin_page = meta.admin_page.map(PluginAdminPageResponse::from_page);
        Self {
            id: meta.id,
            name: meta.name,
            version: meta.version,
            authors: meta.authors,
            description: meta.description,
            state,
            dependencies: meta
                .dependencies
                .into_iter()
                .map(|d| PluginDependencyResponse {
                    id: d.id,
                    optional: d.optional,
                })
                .collect(),
            runtime,
            admin_page,
        }
    }
}

#[derive(Serialize)]
pub struct PluginAdminPageResponse {
    pub version: u16,
    pub slug: String,
    pub title: String,
    pub description: Option<String>,
    pub fields: Vec<PluginAdminFieldResponse>,
    pub components: Vec<PluginAdminComponentResponse>,
    pub actions: Vec<PluginAdminActionResponse>,
}

impl PluginAdminPageResponse {
    fn from_page(page: PluginAdminPage) -> Self {
        Self {
            version: page.version,
            slug: page.slug,
            title: page.title,
            description: page.description,
            fields: page
                .fields
                .into_iter()
                .map(PluginAdminFieldResponse::from_field)
                .collect(),
            components: page
                .components
                .into_iter()
                .map(PluginAdminComponentResponse::from)
                .collect(),
            actions: page
                .actions
                .into_iter()
                .map(PluginAdminActionResponse::from)
                .collect(),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginAdminComponentResponse {
    Stack {
        children: Vec<Self>,
    },
    Grid {
        columns: u8,
        children: Vec<Self>,
    },
    Card {
        title: String,
        children: Vec<Self>,
    },
    Text {
        content: String,
    },
    Callout {
        content: String,
        tone: &'static str,
    },
    Metric {
        label: String,
        value_path: String,
        note: Option<String>,
    },
    Table {
        data_path: String,
        columns: Vec<PluginAdminColumnResponse>,
        empty_text: Option<String>,
        row_action: Option<String>,
    },
    Button {
        label: String,
        action: String,
        destructive: bool,
    },
}

impl From<PluginAdminComponent> for PluginAdminComponentResponse {
    fn from(component: PluginAdminComponent) -> Self {
        match component {
            PluginAdminComponent::Stack { children } => Self::Stack {
                children: children.into_iter().map(Self::from).collect(),
            },
            PluginAdminComponent::Grid { columns, children } => Self::Grid {
                columns,
                children: children.into_iter().map(Self::from).collect(),
            },
            PluginAdminComponent::Card { title, children } => Self::Card {
                title,
                children: children.into_iter().map(Self::from).collect(),
            },
            PluginAdminComponent::Text { content } => Self::Text { content },
            PluginAdminComponent::Callout { content, tone } => Self::Callout {
                content,
                tone: match tone {
                    PluginAdminTone::Neutral => "neutral",
                    PluginAdminTone::Good => "good",
                    PluginAdminTone::Warn => "warn",
                    PluginAdminTone::Bad => "bad",
                    PluginAdminTone::Info => "info",
                },
            },
            PluginAdminComponent::Metric {
                label,
                value_path,
                note,
            } => Self::Metric {
                label,
                value_path,
                note,
            },
            PluginAdminComponent::Table {
                data_path,
                columns,
                empty_text,
                row_action,
            } => Self::Table {
                data_path,
                columns: columns.into_iter().map(Into::into).collect(),
                empty_text,
                row_action,
            },
            PluginAdminComponent::Button {
                label,
                action,
                destructive,
            } => Self::Button {
                label,
                action,
                destructive,
            },
        }
    }
}

#[derive(Serialize)]
pub struct PluginAdminColumnResponse {
    pub key: String,
    pub label: String,
}

impl From<PluginAdminColumn> for PluginAdminColumnResponse {
    fn from(column: PluginAdminColumn) -> Self {
        Self {
            key: column.key,
            label: column.label,
        }
    }
}

#[derive(Serialize)]
pub struct PluginAdminActionResponse {
    pub id: String,
    pub label: String,
    pub confirmation: Option<String>,
    pub destructive: bool,
}

impl From<PluginAdminAction> for PluginAdminActionResponse {
    fn from(action: PluginAdminAction) -> Self {
        Self {
            id: action.id,
            label: action.label,
            confirmation: action.confirmation,
            destructive: action.destructive,
        }
    }
}

#[derive(Serialize)]
pub struct PluginAdminFieldResponse {
    pub key: String,
    pub label: String,
    pub description: Option<String>,
    pub kind: &'static str,
    pub minimum: Option<u64>,
    pub maximum: Option<u64>,
    pub default_integer: Option<u64>,
    pub default_strings: Vec<String>,
    pub max_items: Option<usize>,
}

impl PluginAdminFieldResponse {
    fn from_field(field: PluginAdminField) -> Self {
        Self {
            key: field.key,
            label: field.label,
            description: field.description,
            kind: match field.kind {
                PluginAdminFieldKind::Integer => "integer",
                PluginAdminFieldKind::UuidList => "uuid_list",
            },
            minimum: field.minimum,
            maximum: field.maximum,
            default_integer: field.default_integer,
            default_strings: field.default_strings,
            max_items: field.max_items,
        }
    }
}

#[derive(Serialize)]
pub struct PluginDependencyResponse {
    pub id: String,
    pub optional: bool,
}

#[derive(Serialize)]
pub struct PluginRuntimeResponse {
    pub health: &'static str,
    pub retry_in_ms: Option<u64>,
    pub generation: u64,
    pub restarts_in_window: u32,
    pub max_restarts: u32,
    pub restart_window_secs: u64,
    pub last_fault: Option<PluginFaultResponse>,
    pub queue: PluginQueueResponse,
}

impl PluginRuntimeResponse {
    fn from_status(status: PluginRuntimeStatus) -> Self {
        let queue = status.queue;
        let recent = queue.recent;
        Self {
            health: status.health.as_str(),
            retry_in_ms: status.health.retry_in().map(millis),
            generation: status.generation,
            restarts_in_window: status.restarts.in_window,
            max_restarts: status.restarts.max,
            restart_window_secs: status.restarts.window.as_secs(),
            last_fault: status.last_fault.map(PluginFaultResponse::from_fault),
            queue: PluginQueueResponse {
                depth: queue.depth,
                capacity: queue.capacity,
                window_secs: recent.span.as_secs(),
                taken: recent.taken,
                peak_depth: recent.peak_depth,
                wait_p50_us: micros(recent.wait_p50),
                wait_p99_us: micros(recent.wait_p99),
                wait_max_us: micros(recent.wait_max),
            },
        }
    }
}

#[derive(Serialize)]
pub struct PluginFaultResponse {
    pub cause: String,
    pub secs_ago: u64,
    pub generation: u64,
}

impl PluginFaultResponse {
    fn from_fault(fault: PluginFault) -> Self {
        Self {
            cause: fault.cause,
            secs_ago: fault.ago.as_secs(),
            generation: fault.generation,
        }
    }
}

#[derive(Serialize)]
pub struct PluginQueueResponse {
    pub depth: usize,
    pub capacity: usize,
    pub window_secs: u64,
    pub taken: u64,
    pub peak_depth: usize,
    pub wait_p50_us: u64,
    pub wait_p99_us: u64,
    pub wait_max_us: u64,
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}
