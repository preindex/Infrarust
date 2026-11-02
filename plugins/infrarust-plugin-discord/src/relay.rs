//! Routed Minecraft/Discord chat and lifecycle notifications.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use infrarust_api::event::bus::{EventBus, EventBusExt};
use infrarust_api::event::{EventPriority, ListenerHandle, ResultedEvent};
use infrarust_api::events::{
    ChatMessageEvent, ChatMessageResult, DisconnectCause, DisconnectEvent, KickedFromServerEvent,
    ServerPostConnectEvent,
};
use infrarust_api::player::Player;
use infrarust_api::services::PlayerRegistry;
use infrarust_api::types::{Component, ServerId};
use twilight_http::Client;
use twilight_model::channel::Message;
use twilight_model::channel::message::AllowedMentions;
use twilight_model::id::Id;
use twilight_model::id::marker::ChannelMarker;

use crate::config::{ChatConfig, MessageTemplates};
use crate::limits::SlidingWindow;

const MAX_MESSAGE_CHARS: usize = 1800;

pub struct ChatBridge {
    http: Arc<Client>,
    players: Arc<dyn PlayerRegistry>,
    channels: HashMap<String, Id<ChannelMarker>>,
    servers: HashMap<u64, String>,
    templates: MessageTemplates,
    limits: SlidingWindow,
}

impl ChatBridge {
    pub fn new(
        http: Arc<Client>,
        players: Arc<dyn PlayerRegistry>,
        config: &ChatConfig,
        templates: MessageTemplates,
    ) -> Result<Self, String> {
        let mut channels = HashMap::new();
        let mut servers = HashMap::new();
        for route in &config.routes {
            let server = route.server.trim();
            if server.is_empty() {
                return Err("Discord chat route has an empty server name".into());
            }
            if route.channel_id == 0 {
                return Err(format!(
                    "Discord chat route for `{server}` has an invalid channel ID"
                ));
            }
            if channels
                .insert(server.to_owned(), Id::new(route.channel_id))
                .is_some()
            {
                return Err(format!(
                    "Discord chat has more than one route for server `{server}`"
                ));
            }
            if servers
                .insert(route.channel_id, server.to_owned())
                .is_some()
            {
                return Err(format!(
                    "Discord channel {} is routed to more than one server",
                    route.channel_id
                ));
            }
        }

        Ok(Self {
            http,
            players,
            channels,
            servers,
            templates,
            limits: SlidingWindow::new(config.messages_per_minute.max(1), Duration::from_secs(60)),
        })
    }

    pub fn subscribe(self: &Arc<Self>, bus: &dyn EventBus) -> Vec<ListenerHandle> {
        let chat = Arc::clone(self);
        let chat_handle =
            bus.subscribe_async::<ChatMessageEvent, _>(EventPriority::LAST, move |event| {
                let chat = Arc::clone(&chat);
                Box::pin(async move {
                    let message = match event.result() {
                        ChatMessageResult::Allow => event.message.as_str(),
                        ChatMessageResult::Modify { message } => message.as_str(),
                        ChatMessageResult::Deny { .. } => return,
                        _ => return,
                    };
                    if let Some(server) = event.server.as_ref() {
                        let key = format!("mc:{}", event.player.profile().uuid);
                        if chat.channels.contains_key(server.as_str()) && chat.limits.allows(&key) {
                            let body = render(
                                &chat.templates.minecraft_to_discord,
                                event.player.profile().username.as_str(),
                                server.as_str(),
                                message,
                            );
                            chat.limits.record(&key);
                            chat.send(server, &body).await;
                        }
                    }
                })
            });

        let joined = Arc::clone(self);
        let join_handle =
            bus.subscribe_async::<ServerPostConnectEvent, _>(EventPriority::LAST, move |event| {
                let bridge = Arc::clone(&joined);
                let player = Arc::clone(&event.player);
                let server = event.server.to_string();
                let previous = event.previous_server.as_ref().map(ToString::to_string);
                Box::pin(async move {
                    if previous.as_deref() == Some(server.as_str()) {
                        return;
                    }
                    let template = if previous.is_some() {
                        &bridge.templates.server_switched
                    } else {
                        &bridge.templates.player_joined
                    };
                    let reason = previous.as_deref().unwrap_or_default();
                    let message = render(template, &player.profile().username, &server, reason);
                    bridge.send_named(&server, &player, &message).await;
                })
            });

        let left = Arc::clone(self);
        let leave_handle =
            bus.subscribe_async::<DisconnectEvent, _>(EventPriority::LAST, move |event| {
                let bridge = Arc::clone(&left);
                let player = Arc::clone(&event.player);
                let server = event.last_server.as_ref().map(ToString::to_string);
                let cause = event.cause.clone();
                Box::pin(async move {
                    let Some(server) = server else { return };
                    if matches!(cause, DisconnectCause::Kicked { .. }) {
                        return;
                    }
                    let message = render(
                        &bridge.templates.player_left,
                        &player.profile().username,
                        &server,
                        "",
                    );
                    bridge.send_named(&server, &player, &message).await;
                })
            });

        let kicked = Arc::clone(self);
        let kick_handle =
            bus.subscribe_async::<KickedFromServerEvent, _>(EventPriority::LAST, move |event| {
                let bridge = Arc::clone(&kicked);
                let player = Arc::clone(&event.player);
                let server = event.server.to_string();
                let reason = event
                    .reason
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| event.cause.as_str().to_owned());
                Box::pin(async move {
                    let message = render(
                        &bridge.templates.player_kicked,
                        &player.profile().username,
                        &server,
                        &reason,
                    );
                    bridge.send_named(&server, &player, &message).await;
                })
            });

        vec![chat_handle, join_handle, leave_handle, kick_handle]
    }

    pub async fn handle_discord_message(&self, message: &Message) {
        if message.author.bot || message.webhook_id.is_some() || message.content.trim().is_empty() {
            return;
        }
        let Some(server) = self.servers.get(&message.channel_id.get()) else {
            return;
        };
        let key = format!("discord:{}", message.author.id);
        if !self.limits.allows(&key) {
            return;
        }
        self.limits.record(&key);
        let body = render(
            &self.templates.discord_to_minecraft,
            &message.author.name,
            server,
            &suppress_mentions(&message.content),
        );
        for player in self.players.get_players_on_server(&ServerId::new(server)) {
            if player.is_active() {
                let _ = player.send_message(Component::text(body.clone()));
            }
        }
    }

    async fn send_named(&self, server: &str, player: &Arc<dyn Player>, body: &str) {
        let key = format!("mc:{}", player.profile().uuid);
        if !self.limits.allows(&key) {
            return;
        }
        self.limits.record(&key);
        self.send(&ServerId::new(server), body).await;
    }

    async fn send(&self, server: &ServerId, body: &str) {
        let Some(channel) = self.channels.get(server.as_str()) else {
            return;
        };
        if let Err(error) = self
            .http
            .create_message(*channel)
            .content(&truncate(body, MAX_MESSAGE_CHARS))
            .allowed_mentions(Some(&AllowedMentions::default()))
            .await
        {
            tracing::warn!(server = %server, error = %error, "Discord relay message failed");
        }
    }
}

fn render(template: &str, player: &str, server: &str, message: &str) -> String {
    let value = template
        .replace("{player}", player)
        .replace("{server}", server)
        .replace("{message}", message)
        .replace("{reason}", message);
    truncate(&value, MAX_MESSAGE_CHARS)
}

fn suppress_mentions(value: &str) -> String {
    value.replace('@', "@\u{200b}")
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let value: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{value}…")
    } else {
        value
    }
}
