//! Native Discord plugin for Infrarust.
//!
//! One plugin owns both halves of the Discord community gate. The Discord bot
//! answers `/link` and `/unlink`, and the gate is bound to the core
//! generic admission-guard API. The
//! gate only sees players the proxy has already verified with Mojang.
//! Backend Minecraft servers run no plugin.

mod backend_events;
mod backend_groups;
pub mod bot;
pub mod config;
pub mod gate;
pub mod limbo;
pub mod limits;
pub mod log_forward;
pub mod membership;
pub mod permissions;
pub mod relay;
pub mod store;
pub mod voice;

use std::sync::{Arc, Mutex, PoisonError};

use infrarust_api::error::PluginError;
use infrarust_api::event::bus::EventBus;
use infrarust_api::event::{BoxFuture, EventPriority, ListenerHandle};
use infrarust_api::events::PostLoginEvent;
use infrarust_api::messaging::ChannelId;
use infrarust_api::plugin::{
    Plugin, PluginAdminAction, PluginAdminColumn, PluginAdminComponent, PluginAdminField,
    PluginAdminPage, PluginAdminTone, PluginContext, PluginMetadata,
};
use infrarust_api::prelude::EventBusExt;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use twilight_http::Client;

use crate::backend_events::BackendEventReceiver;
use crate::backend_groups::GroupSync;
use crate::bot::{Bot, sync_nickname_to_guilds};
use crate::config::{ConfigError, DiscordConfig, LogForwardConfig};
use crate::gate::DiscordGate;
use crate::limbo::DiscordLinkHandler;
use crate::limits::SlidingWindow;
use crate::membership::MembershipChecker;
use crate::permissions::DiscordPermissionProvider;
use crate::relay::ChatBridge;
use crate::store::LinkBook;
use crate::voice::VoiceProximity;

/// The plugin id, which also owns this plugin's admission namespace.
pub const PLUGIN_ID: &str = "discord";

struct Runtime {
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
    log_forward: Vec<JoinHandle<()>>,
    group_sync_task: Option<JoinHandle<()>>,
    event_bus: Arc<dyn EventBus>,
    listeners: Vec<ListenerHandle>,
    #[allow(dead_code)]
    registered_channel: ChannelId,
}

struct ListenerGuard {
    event_bus: Arc<dyn EventBus>,
    listeners: Vec<ListenerHandle>,
    armed: bool,
}

impl ListenerGuard {
    fn new(event_bus: Arc<dyn EventBus>) -> Self {
        Self {
            event_bus,
            listeners: Vec::new(),
            armed: true,
        }
    }

    fn push(&mut self, handle: ListenerHandle) {
        self.listeners.push(handle);
    }

    fn into_runtime_parts(mut self) -> (Arc<dyn EventBus>, Vec<ListenerHandle>) {
        self.armed = false;
        (
            Arc::clone(&self.event_bus),
            std::mem::take(&mut self.listeners),
        )
    }
}

impl Drop for ListenerGuard {
    fn drop(&mut self) {
        if self.armed {
            for handle in self.listeners.drain(..) {
                self.event_bus.unsubscribe(handle);
            }
        }
    }
}

pub struct DiscordPlugin {
    runtime: Mutex<Option<Runtime>>,
    admin_links: tokio::sync::RwLock<Option<Arc<LinkBook>>>,
}

impl DiscordPlugin {
    pub fn new() -> Self {
        Self {
            runtime: Mutex::new(None),
            admin_links: tokio::sync::RwLock::new(None),
        }
    }
}

impl Default for DiscordPlugin {
    fn default() -> Self {
        Self::new()
    }
}

fn init_err(err: impl std::fmt::Display) -> PluginError {
    PluginError::InitFailed(err.to_string())
}

impl Plugin for DiscordPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata::new(PLUGIN_ID, "Discord", env!("CARGO_PKG_VERSION"))
            .author("Infrarust")
            .description("Discord bot and community gate for protected servers")
            .admin_page(
                PluginAdminPage::new(1, "settings", "Community gate")
                    .description(
                        "Manage link limits, membership cache durations, and UUID bypasses. \
                         Changes take effect after restarting Infrarust. Bot tokens are managed \
                         through the configured environment variable, secret file, or plugin \
                         configuration and are never shown here.",
                    )
                    .field(
                        PluginAdminField::integer(
                            "links.code_ttl_seconds",
                            "Link code lifetime (seconds)",
                            600,
                            1,
                            86_400,
                        )
                        .description("How long an unused Discord link code remains valid."),
                    )
                    .field(
                        PluginAdminField::integer(
                            "links.max_failed_attempts",
                            "Failed attempts",
                            5,
                            1,
                            100,
                        )
                        .description("Maximum failed link attempts in the configured window."),
                    )
                    .field(PluginAdminField::integer(
                        "links.failure_window_seconds",
                        "Failure window (seconds)",
                        600,
                        1,
                        86_400,
                    ))
                    .field(PluginAdminField::integer(
                        "links.max_pending_codes",
                        "Maximum pending codes",
                        10_000,
                        1,
                        100_000,
                    ))
                    .field(
                        PluginAdminField::uuid_list("gate.bypass_uuids", "Bypass UUIDs", 512)
                            .description(
                                "Mojang UUIDs exempt from Discord linking and membership checks.",
                            ),
                    )
                    .field(PluginAdminField::integer(
                        "membership.positive_cache_seconds",
                        "Positive membership cache (seconds)",
                        300,
                        0,
                        86_400,
                    ))
                    .field(PluginAdminField::integer(
                        "membership.negative_cache_seconds",
                        "Negative membership cache (seconds)",
                        60,
                        0,
                        86_400,
                    ))
                    .action(
                        PluginAdminAction::new("unlink", "Unlink")
                            .confirmation("Remove this Discord account link?")
                            .destructive(),
                    )
                    .component(PluginAdminComponent::Grid {
                        columns: 2,
                        children: vec![
                            PluginAdminComponent::Card {
                                title: "Community gate".into(),
                                children: vec![
                                    PluginAdminComponent::Metric {
                                        label: "Linked accounts".into(),
                                        value_path: "linked_count".into(),
                                        note: Some("Persisted Discord account links".into()),
                                    },
                                    PluginAdminComponent::Callout {
                                        content: "Bot credentials stay in the configured secret source and are never exposed here.".into(),
                                        tone: PluginAdminTone::Info,
                                    },
                                ],
                            },
                            PluginAdminComponent::Card {
                                title: "Linked accounts".into(),
                                children: vec![PluginAdminComponent::Table {
                                    data_path: "links".into(),
                                    columns: vec![
                                        PluginAdminColumn::new("uuid", "Minecraft UUID"),
                                        PluginAdminColumn::new("discord_id", "Discord ID"),
                                        PluginAdminColumn::new("linked_at", "Linked at"),
                                    ],
                                    empty_text: Some("No linked accounts yet.".into()),
                                    row_action: Some("unlink".into()),
                                }],
                            },
                        ],
                    }),
            )
    }

    fn on_enable<'a>(
        &'a self,
        ctx: &'a dyn PluginContext,
    ) -> BoxFuture<'a, Result<(), PluginError>> {
        Box::pin(async move {
            let data_dir = ctx.data_dir();
            let config = DiscordConfig::load_or_create(&data_dir.join("config.toml"))
                .await
                .map_err(init_err)?;
            let token = match config.resolve_token() {
                Ok(token) => Some(token),
                Err(ConfigError::MissingToken) => {
                    tracing::info!(
                        "Discord plugin is loaded without a bot token; configured bypass UUIDs remain allowed, while other Discord-gated connections are refused until INFRARUST_BOT_TOKEN or [bot].token is configured"
                    );
                    None
                }
                Err(err) => return Err(init_err(err)),
            };

            let links = Arc::new(
                LinkBook::open(
                    &data_dir,
                    config.links.code_ttl(),
                    config.links.max_pending_codes,
                )
                .await
                .map_err(init_err)?,
            );
            *self.admin_links.write().await = Some(Arc::clone(&links));
            let (http, membership) = if let Some(token) = &token {
                let http = Arc::new(Client::new(token.clone()));
                let membership = Arc::new(MembershipChecker::new(
                    Arc::clone(&http),
                    &config.membership,
                ));
                (Some(http), Some(membership))
            } else {
                (None, None)
            };
            let bot_available = membership.is_some();
            let group_membership = membership.clone();
            let gate = Arc::new(DiscordGate::new(
                Arc::clone(&links),
                membership.clone(),
                &config.gate,
                &config.role_gate,
            ));
            ctx.register_limbo_handler(Box::new(DiscordLinkHandler::new(
                Arc::clone(&links),
                bot_available,
                &config.gate,
            )))
            .map_err(init_err)?;
            ctx.register_admission_guard(gate).map_err(init_err)?;

            if !config.role_permissions.guild_roles.is_empty() {
                if let Some(membership) = membership {
                    if let Err(err) =
                        ctx.register_permission_provider(Arc::new(DiscordPermissionProvider::new(
                            Arc::clone(&links),
                            membership,
                            config.role_permissions.clone(),
                        )))
                    {
                        tracing::warn!(error = %err, "Discord role permission provider was not registered");
                    }
                } else {
                    tracing::warn!(
                        "Discord role permission mappings are configured but the bot token is missing"
                    );
                }
            }

            let event_bus = ctx.event_bus();
            let mut listener_guard = ListenerGuard::new(Arc::clone(&event_bus));
            let backend_events = Arc::new(
                BackendEventReceiver::load(&data_dir.join("backend-events.toml"), http.clone())
                    .await
                    .map_err(init_err)?,
            );
            let registered_channel = ChannelId::modern("infrarust:discord").map_err(init_err)?;
            ctx.channel_registrar().register(registered_channel.clone());
            listener_guard.push(backend_events.subscribe(event_bus.as_ref()));
            let group_sync = GroupSync::new(
                backend_events.group_sync_routes(),
                Arc::clone(&links),
                group_membership,
                ctx.server_messenger(),
            )
            .map_err(init_err)?;
            if let Some(group_sync) = &group_sync {
                listener_guard.push(group_sync.subscribe(event_bus.as_ref()));
            }

            let bridge = if let Some(http) = &http {
                Some(Arc::new(
                    ChatBridge::new(
                        Arc::clone(http),
                        ctx.player_registry(),
                        &config.chat,
                        config.messages.clone(),
                    )
                    .map_err(init_err)?,
                ))
            } else {
                None
            };
            if let Some(bridge) = &bridge {
                for handle in bridge.subscribe(event_bus.as_ref()) {
                    listener_guard.push(handle);
                }
            }
            let mut task = None;
            let mut log_forward = Vec::new();
            let cancel = ctx.proxy_shutdown().child_token();
            if let (Some(token), Some(http), Some(bridge)) = (token, http, bridge) {
                if config.log_forward.enabled {
                    log_forward = start_log_forward(
                        &config.log_forward,
                        Arc::clone(&http),
                        ctx,
                        cancel.clone(),
                    );
                }
                let voice = if config.voice.enabled {
                    Some(Arc::new(
                        VoiceProximity::new(
                            config.voice.clone(),
                            Arc::clone(&links),
                            backend_events.position_store(),
                            Arc::clone(&http),
                        )
                        .map_err(init_err)?,
                    ))
                } else {
                    None
                };
                if config.nicknames.enabled {
                    let links_for_login = Arc::clone(&links);
                    let http_for_login = Arc::clone(&http);
                    let nickname_config = config.nicknames.clone();
                    let handle = event_bus.subscribe_async::<PostLoginEvent, _>(
                        EventPriority::NORMAL,
                        move |event| {
                            let uuid = event.profile.uuid;
                            let username = event.profile.username.clone();
                            let links = Arc::clone(&links_for_login);
                            let http = Arc::clone(&http_for_login);
                            let config = nickname_config.clone();
                            Box::pin(async move {
                                if let Some(discord_id) = links.linked_discord(uuid).await
                                    && let Some(user) = membership::parse_user(&discord_id)
                                {
                                    sync_nickname_to_guilds(http, user, config, uuid, username)
                                        .await;
                                }
                            })
                        },
                    );
                    listener_guard.push(handle);
                }
                let failures = Arc::new(SlidingWindow::new(
                    config.links.max_failed_attempts,
                    config.links.failure_window(),
                ));
                let bot = Bot::new(
                    token,
                    http,
                    config.bot.application_id,
                    links,
                    failures,
                    bridge,
                    ctx.ban_service(),
                    config.moderation.clone(),
                    config.nicknames.clone(),
                    ctx.player_registry(),
                    ctx.command_manager(),
                    config.console.clone(),
                    config.messages.clone(),
                    voice,
                );
                task = Some(tokio::spawn(bot.run(cancel.clone())));
            }
            let group_sync_task = group_sync
                .map(|sync| tokio::spawn(sync.run_resync(ctx.player_registry(), cancel.clone())));
            let (event_bus, listeners) = listener_guard.into_runtime_parts();
            *self.runtime.lock().unwrap_or_else(PoisonError::into_inner) = Some(Runtime {
                cancel,
                task,
                log_forward,
                group_sync_task,
                event_bus,
                listeners,
                registered_channel,
            });
            Ok(())
        })
    }

    fn on_disable(&self) -> BoxFuture<'_, Result<(), PluginError>> {
        Box::pin(async move {
            let runtime = self
                .runtime
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if let Some(runtime) = runtime {
                runtime.cancel.cancel();
                for listener in runtime.listeners {
                    runtime.event_bus.unsubscribe(listener);
                }
                if let Some(task) = runtime.task {
                    if let Err(err) = task.await {
                        tracing::error!(error = %err, "Discord bot task ended abnormally");
                    }
                }
                for handle in runtime.log_forward {
                    let _ = handle.await;
                }
                if let Some(task) = runtime.group_sync_task
                    && let Err(err) = task.await
                {
                    tracing::error!(error = %err, "Discord group sync task ended abnormally");
                }
            }
            *self.admin_links.write().await = None;
            Ok(())
        })
    }

    fn admin_data(&self) -> BoxFuture<'_, Result<serde_json::Value, PluginError>> {
        Box::pin(async move {
            let links = self.admin_links.read().await.clone().ok_or_else(|| {
                PluginError::Service(infrarust_api::error::ServiceError::OperationFailed(
                    "Discord plugin is not running".into(),
                ))
            })?;
            let records = links.list_links().await;
            let linked_count = records.len();
            Ok(serde_json::json!({
                "linked_count": linked_count,
                "links": records.into_iter().map(|record| serde_json::json!({
                    "uuid": record.uuid.to_string(),
                    "discord_id": record.discord_id,
                    "linked_at": i64::try_from(record.linked_at)
                        .ok()
                        .and_then(|seconds| chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, 0))
                        .map(|instant| instant.to_rfc3339())
                        .unwrap_or_else(|| record.linked_at.to_string()),
                })).collect::<Vec<_>>(),
            }))
        })
    }

    fn admin_action<'a>(
        &'a self,
        action: &'a str,
        input: serde_json::Value,
    ) -> BoxFuture<'a, Result<serde_json::Value, PluginError>> {
        Box::pin(async move {
            if action != "unlink" {
                return Err(PluginError::Service(
                    infrarust_api::error::ServiceError::NotFound(format!(
                        "Discord admin action '{action}'"
                    )),
                ));
            }
            let discord_id = input
                .get("discord_id")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty() && value.len() <= 20)
                .ok_or_else(|| {
                    PluginError::Service(infrarust_api::error::ServiceError::OperationFailed(
                        "a Discord ID is required".into(),
                    ))
                })?;
            let links = self.admin_links.read().await.clone().ok_or_else(|| {
                PluginError::Service(infrarust_api::error::ServiceError::OperationFailed(
                    "Discord plugin is not running".into(),
                ))
            })?;
            let removed = links
                .unlink(discord_id)
                .await
                .map_err(|error| PluginError::Other(Box::new(error)))?;
            Ok(serde_json::json!({ "removed": removed.is_some() }))
        })
    }
}

/// Starts log forwarding when the proxy has granted `log-read`. Otherwise it
/// logs why forwarding is off and returns no tasks.
fn start_log_forward(
    config: &LogForwardConfig,
    http: Arc<Client>,
    ctx: &dyn PluginContext,
    cancel: CancellationToken,
) -> Vec<JoinHandle<()>> {
    let Some(logs) = ctx.log_service() else {
        tracing::warn!(
            "Discord log forwarding is enabled but the plugin lacks log-read; add permissions = [\"log-read\"] under [plugins.discord] in the proxy config"
        );
        return Vec::new();
    };
    match log_forward::start(config, http, logs, cancel) {
        Ok(handles) => {
            tracing::info!(
                channel_id = config.channel_id,
                min_level = %config.min_level,
                targets = ?config.targets,
                "Discord log forwarding is enabled"
            );
            handles
        }
        Err(err) => {
            tracing::warn!(error = %err, "Discord log forwarding is disabled");
            Vec::new()
        }
    }
}
