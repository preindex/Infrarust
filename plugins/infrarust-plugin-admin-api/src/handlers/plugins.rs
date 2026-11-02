use std::collections::{BTreeMap, HashSet};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use toml_edit::{Array, DocumentMut};

use crate::dto::plugin::PluginResponse;
use crate::error::ApiError;
use crate::response::{ApiResponse, RestartAwareResult, ok};
use crate::state::ApiState;
use infrarust_api::plugin::{PluginAdminField, PluginAdminFieldKind, PluginAdminPage};

pub async fn list(
    State(state): State<Arc<ApiState>>,
) -> Result<Json<ApiResponse<Vec<PluginResponse>>>, ApiError> {
    let plugins: Vec<PluginResponse> = state
        .plugin_registry
        .list_plugin_info()
        .into_iter()
        .map(PluginResponse::from_info)
        .collect();

    Ok(ok(plugins))
}

pub async fn get(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<PluginResponse>>, ApiError> {
    let info = state
        .plugin_registry
        .plugin_info(&id)
        .ok_or_else(|| ApiError::NotFound(format!("Plugin '{id}' not found")))?;

    Ok(ok(PluginResponse::from_info(info)))
}

#[derive(Serialize)]
pub struct PluginAdminConfig {
    pub values: BTreeMap<String, JsonValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginAdminConfigUpdate {
    pub values: BTreeMap<String, JsonValue>,
}

#[derive(Serialize)]
pub struct PluginAdminData {
    pub data: JsonValue,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginAdminActionRequest {
    #[serde(default)]
    pub input: JsonValue,
}

pub async fn get_admin_config(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<PluginAdminConfig>>, ApiError> {
    let page = plugin_page(&state, &id)?;
    let _guard = state.plugin_config_write.lock().await;
    let path = plugin_config_path(&state, &id).await?;
    let document = read_config(&path).await?;
    let mut values = BTreeMap::new();

    for field in &page.fields {
        values.insert(field.key.clone(), read_field(&document, field)?);
    }

    Ok(ok(PluginAdminConfig { values }))
}

pub async fn update_admin_config(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
    Json(update): Json<PluginAdminConfigUpdate>,
) -> Result<Json<ApiResponse<RestartAwareResult>>, ApiError> {
    let page = plugin_page(&state, &id)?;
    if update.values.len() > page.fields.len() {
        return Err(ApiError::BadRequest("Too many settings supplied".into()));
    }
    let fields = page
        .fields
        .iter()
        .map(|field| (field.key.as_str(), field))
        .collect::<BTreeMap<_, _>>();
    let mut validated = Vec::with_capacity(update.values.len());

    for (key, value) in update.values {
        let field = fields
            .get(key.as_str())
            .ok_or_else(|| ApiError::BadRequest(format!("Unknown setting '{key}'")))?;
        validated.push(((**field).clone(), validate_field_value(field, value)?));
    }

    let _guard = state.plugin_config_write.lock().await;
    let path = plugin_config_path(&state, &id).await?;
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|error| config_io_error(error, "read"))?;
    let mut document = content
        .parse::<DocumentMut>()
        .map_err(|_| ApiError::BadRequest("Plugin config is not valid TOML".into()))?;

    for (field, value) in &validated {
        set_field(&mut document, field, value);
    }

    tokio::fs::write(&path, document.to_string())
        .await
        .map_err(|error| config_io_error(error, "write"))?;

    tracing::info!(
        target: "audit",
        action = "plugin_admin_config_write",
        source = "admin_api",
        plugin = %id,
        fields = ?validated.iter().map(|(field, _)| &field.key).collect::<Vec<_>>(),
        "Plugin settings updated via Admin API"
    );

    Ok(ok(RestartAwareResult {
        success: true,
        message: "Plugin config saved; restart Infrarust to apply the changes".into(),
        requires_restart: true,
    }))
}

pub async fn get_admin_data(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<PluginAdminData>>, ApiError> {
    let _page = plugin_page(&state, &id)?;
    let data = state
        .plugin_registry
        .plugin_admin_data(&id)
        .await
        .map_err(plugin_admin_error)?;
    validate_plugin_payload_size(&data)?;
    Ok(ok(PluginAdminData { data }))
}

pub async fn run_admin_action(
    State(state): State<Arc<ApiState>>,
    Path((id, action)): Path<(String, String)>,
    Json(request): Json<PluginAdminActionRequest>,
) -> Result<Json<ApiResponse<JsonValue>>, ApiError> {
    let page = plugin_page(&state, &id)?;
    if !page.actions.iter().any(|declared| declared.id == action) {
        return Err(ApiError::NotFound(format!(
            "Plugin admin action '{action}' not found"
        )));
    }
    let result = state
        .plugin_registry
        .plugin_admin_action(&id, &action, request.input)
        .await
        .map_err(plugin_admin_error)?;
    validate_plugin_payload_size(&result)?;
    tracing::info!(
        target: "audit",
        action = "plugin_admin_action",
        source = "admin_api",
        plugin = %id,
        operation = %action,
        "Plugin admin operation invoked"
    );
    Ok(ok(result))
}

fn validate_plugin_payload_size(value: &JsonValue) -> Result<(), ApiError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| ApiError::Internal("Plugin admin response is not serializable".into()))?;
    if bytes.len() > 1024 * 1024 {
        return Err(ApiError::Internal(
            "Plugin admin response exceeds the 1 MiB limit".into(),
        ));
    }
    Ok(())
}

fn plugin_admin_error(error: infrarust_api::error::PluginError) -> ApiError {
    tracing::warn!(%error, "Plugin admin operation failed");
    ApiError::Internal("Plugin admin operation failed".into())
}

fn plugin_page(state: &ApiState, id: &str) -> Result<PluginAdminPage, ApiError> {
    let info = state
        .plugin_registry
        .plugin_info(id)
        .ok_or_else(|| ApiError::NotFound(format!("Plugin '{id}' not found")))?;
    let page = info
        .metadata
        .admin_page
        .ok_or_else(|| ApiError::NotFound(format!("Plugin '{id}' has no admin page")))?;
    validate_admin_page(&page)?;
    Ok(page)
}

fn validate_admin_page(page: &PluginAdminPage) -> Result<(), ApiError> {
    let mut keys = HashSet::new();
    if page.version != 1
        || page.slug.is_empty()
        || page.slug.len() > 64
        || !page
            .slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || page.fields.len() > 64
        || page.actions.len() > 64
        || page.components.len() > 256
    {
        return Err(ApiError::Internal(
            "Plugin admin page uses an unsupported schema".into(),
        ));
    }

    let mut actions = HashSet::new();
    for action in &page.actions {
        if !valid_admin_id(&action.id) || !actions.insert(action.id.as_str()) {
            return Err(ApiError::Internal(
                "Plugin admin page contains an invalid or duplicate action".into(),
            ));
        }
    }
    let mut components_remaining = 256;
    for component in &page.components {
        validate_component(component, &actions, &mut components_remaining, 0)?;
    }

    for field in &page.fields {
        let parts = field.key.split('.').collect::<Vec<_>>();
        if parts.len() != 2
            || parts.iter().any(|part| {
                part.is_empty()
                    || !part
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            })
            || [
                "token",
                "password",
                "secret",
                "credential",
                "api_key",
                "private_key",
                "access_key",
                "signing_key",
                "encryption_key",
            ]
            .iter()
            .any(|secret| field.key.to_ascii_lowercase().contains(secret))
            || field
                .key
                .split('.')
                .any(|part| part == "key" || part.ends_with("_key"))
            || !keys.insert(field.key.as_str())
        {
            return Err(ApiError::Internal(
                "Plugin admin page contains an unsafe or duplicate setting".into(),
            ));
        }

        match field.kind {
            PluginAdminFieldKind::Integer
                if field.minimum.zip(field.maximum).is_none_or(|(min, max)| {
                    min > max
                        || max > i64::MAX as u64
                        || field
                            .default_integer
                            .is_none_or(|default| default < min || default > max)
                }) =>
            {
                return Err(ApiError::Internal(
                    "Plugin admin integer setting has an invalid range".into(),
                ));
            }
            PluginAdminFieldKind::UuidList if field.max_items.is_none_or(|max| max > 512) => {
                return Err(ApiError::Internal(
                    "Plugin admin UUID list has an invalid bound".into(),
                ));
            }
            PluginAdminFieldKind::UuidList
                if field.default_strings.len() > field.max_items.unwrap_or(0)
                    || field
                        .default_strings
                        .iter()
                        .any(|value| uuid::Uuid::parse_str(value).is_err()) =>
            {
                return Err(ApiError::Internal(
                    "Plugin admin UUID list has invalid defaults".into(),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn valid_admin_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        })
}

fn validate_component(
    component: &infrarust_api::plugin::PluginAdminComponent,
    actions: &HashSet<&str>,
    remaining: &mut usize,
    depth: usize,
) -> Result<(), ApiError> {
    use infrarust_api::plugin::PluginAdminComponent;
    if depth > 8 || *remaining == 0 {
        return Err(ApiError::Internal(
            "Plugin admin component tree exceeds its bounds".into(),
        ));
    }
    *remaining -= 1;
    let children = match component {
        PluginAdminComponent::Stack { children } | PluginAdminComponent::Card { children, .. } => {
            Some(children)
        }
        PluginAdminComponent::Grid { columns, children } => {
            if !(1..=4).contains(columns) {
                return Err(ApiError::Internal(
                    "Plugin admin grid column count must be between one and four".into(),
                ));
            }
            Some(children)
        }
        PluginAdminComponent::Table {
            data_path,
            columns,
            row_action,
            ..
        } => {
            if !valid_data_path(data_path)
                || columns.is_empty()
                || columns.len() > 32
                || columns.iter().any(|column| !valid_data_path(&column.key))
                || row_action
                    .as_ref()
                    .is_some_and(|action| !actions.contains(action.as_str()))
            {
                return Err(ApiError::Internal(
                    "Plugin admin table uses an invalid data path or column".into(),
                ));
            }
            None
        }
        PluginAdminComponent::Metric { value_path, .. } if !valid_data_path(value_path) => {
            return Err(ApiError::Internal(
                "Plugin admin metric uses an invalid data path".into(),
            ));
        }
        PluginAdminComponent::Button { action, .. } if !actions.contains(action.as_str()) => {
            return Err(ApiError::Internal(
                "Plugin admin button references an undeclared action".into(),
            ));
        }
        _ => None,
    };
    if let Some(children) = children {
        if children.len() > 64 {
            return Err(ApiError::Internal(
                "Plugin admin container has too many children".into(),
            ));
        }
        for child in children {
            validate_component(child, actions, remaining, depth + 1)?;
        }
    }
    Ok(())
}

fn valid_data_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 128
        && path.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        })
}

async fn plugin_config_path(state: &ApiState, id: &str) -> Result<PathBuf, ApiError> {
    if id.is_empty()
        || !id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
    {
        return Err(ApiError::BadRequest("Invalid plugin id".into()));
    }

    let root = tokio::fs::canonicalize(&state.plugin_data_root)
        .await
        .map_err(|error| config_io_error(error, "resolve plugin data root"))?;
    let directory = state.plugin_data_root.join(id);
    let metadata = tokio::fs::symlink_metadata(&directory)
        .await
        .map_err(|error| config_io_error(error, "resolve plugin data directory"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ApiError::Forbidden(
            "Plugin data directory is not a regular directory".into(),
        ));
    }
    let directory = tokio::fs::canonicalize(directory)
        .await
        .map_err(|error| config_io_error(error, "resolve plugin data directory"))?;
    if !directory.starts_with(&root) || directory == root {
        return Err(ApiError::Forbidden(
            "Plugin data directory is outside the configured root".into(),
        ));
    }

    let config = directory.join("config.toml");
    let config_metadata = tokio::fs::symlink_metadata(&config)
        .await
        .map_err(|error| config_io_error(error, "resolve plugin config"))?;
    if config_metadata.file_type().is_symlink() || !config_metadata.is_file() {
        return Err(ApiError::Forbidden(
            "Plugin config is not a regular file".into(),
        ));
    }
    if config_metadata.len() > 1024 * 1024 {
        return Err(ApiError::BadRequest(
            "Plugin config exceeds the 1 MiB admin limit".into(),
        ));
    }
    let canonical_config = tokio::fs::canonicalize(config)
        .await
        .map_err(|error| config_io_error(error, "resolve plugin config"))?;
    if !canonical_config.starts_with(&directory) {
        return Err(ApiError::Forbidden(
            "Plugin config is outside its data directory".into(),
        ));
    }
    Ok(canonical_config)
}

async fn read_config(path: &FsPath) -> Result<toml::Value, ApiError> {
    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|error| config_io_error(error, "read"))?;
    toml::from_str(&content)
        .map_err(|_| ApiError::BadRequest("Plugin config is not valid TOML".into()))
}

fn read_field(document: &toml::Value, field: &PluginAdminField) -> Result<JsonValue, ApiError> {
    let (section, name) = field.key.split_once('.').ok_or_else(|| {
        ApiError::Internal("Plugin admin page contains an invalid setting".into())
    })?;
    let value = document.get(section).and_then(|table| table.get(name));
    match field.kind {
        PluginAdminFieldKind::Integer => {
            let value = match value {
                Some(value) => value
                    .as_integer()
                    .and_then(|value| u64::try_from(value).ok())
                    .ok_or_else(|| {
                        ApiError::BadRequest(format!("Invalid value for {}", field.key))
                    })?,
                None => field.default_integer.ok_or_else(|| {
                    ApiError::BadRequest(format!("Invalid value for {}", field.key))
                })?,
            };
            if value < field.minimum.unwrap_or(0) || value > field.maximum.unwrap_or(u64::MAX) {
                return Err(ApiError::BadRequest(format!(
                    "{} is outside the allowed range",
                    field.key
                )));
            }
            Ok(JsonValue::from(value))
        }
        PluginAdminFieldKind::UuidList => {
            let values = match value {
                Some(value) => value
                    .as_array()
                    .ok_or_else(|| {
                        ApiError::BadRequest(format!("Invalid value for {}", field.key))
                    })?
                    .iter()
                    .map(|value| {
                        let text = value.as_str().ok_or_else(|| {
                            ApiError::BadRequest(format!("Invalid value for {}", field.key))
                        })?;
                        let uuid = uuid::Uuid::parse_str(text).map_err(|_| {
                            ApiError::BadRequest(format!("Invalid UUID in {}", field.key))
                        })?;
                        Ok(JsonValue::String(uuid.to_string()))
                    })
                    .collect::<Result<Vec<_>, ApiError>>()?,
                None => field
                    .default_strings
                    .iter()
                    .map(|value| JsonValue::String(value.clone()))
                    .collect(),
            };
            if values.len() > field.max_items.unwrap_or(0) {
                return Err(ApiError::BadRequest(format!(
                    "Too many entries for {}",
                    field.key
                )));
            }
            Ok(JsonValue::Array(values))
        }
    }
}

enum ValidatedFieldValue {
    Integer(u64),
    UuidList(Vec<String>),
}

fn validate_field_value(
    field: &PluginAdminField,
    value: JsonValue,
) -> Result<ValidatedFieldValue, ApiError> {
    match field.kind {
        PluginAdminFieldKind::Integer => {
            let value = value
                .as_u64()
                .ok_or_else(|| ApiError::BadRequest(format!("{} must be an integer", field.key)))?;
            if value < field.minimum.unwrap_or(0) || value > field.maximum.unwrap_or(u64::MAX) {
                return Err(ApiError::BadRequest(format!(
                    "{} is outside the allowed range",
                    field.key
                )));
            }
            Ok(ValidatedFieldValue::Integer(value))
        }
        PluginAdminFieldKind::UuidList => {
            let values = value.as_array().ok_or_else(|| {
                ApiError::BadRequest(format!("{} must be a UUID list", field.key))
            })?;
            if values.len() > field.max_items.unwrap_or(0) {
                return Err(ApiError::BadRequest(format!(
                    "Too many entries for {}",
                    field.key
                )));
            }
            let mut output = Vec::with_capacity(values.len());
            let mut unique = HashSet::new();
            for value in values {
                let text = value.as_str().ok_or_else(|| {
                    ApiError::BadRequest(format!("{} must contain UUID strings", field.key))
                })?;
                if text.len() > 36 {
                    return Err(ApiError::BadRequest(format!(
                        "Invalid UUID in {}",
                        field.key
                    )));
                }
                let uuid = uuid::Uuid::parse_str(text)
                    .map_err(|_| ApiError::BadRequest(format!("Invalid UUID in {}", field.key)))?
                    .to_string();
                if !unique.insert(uuid.clone()) {
                    return Err(ApiError::BadRequest(format!(
                        "Duplicate UUID in {}",
                        field.key
                    )));
                }
                output.push(uuid);
            }
            Ok(ValidatedFieldValue::UuidList(output))
        }
    }
}

fn set_field(
    document: &mut DocumentMut,
    field: &PluginAdminField,
    value_to_set: &ValidatedFieldValue,
) {
    let (section, name) = field.key.split_once('.').expect("validated two-part key");
    if !document.as_table().contains_key(section) {
        document[section] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    match value_to_set {
        ValidatedFieldValue::Integer(value) => {
            document[section][name] = toml_edit::value(*value as i64);
        }
        ValidatedFieldValue::UuidList(values) => {
            let mut array = Array::new();
            for value in values {
                array.push(value.as_str());
            }
            document[section][name] = toml_edit::value(array);
        }
    }
}

fn config_io_error(error: std::io::Error, operation: &str) -> ApiError {
    match error.kind() {
        std::io::ErrorKind::NotFound => {
            ApiError::NotFound("Plugin configuration file not found".into())
        }
        std::io::ErrorKind::PermissionDenied => {
            ApiError::Forbidden("Plugin configuration access denied".into())
        }
        _ => {
            tracing::error!(%error, operation, "Plugin config access failed");
            ApiError::Internal("Failed to access plugin configuration".into())
        }
    }
}
