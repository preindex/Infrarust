//! Discord bot: registers the `/link` and `/unlink` slash commands and answers
//! them over the Gateway.
//!
//! Replies are ephemeral, so a one-time link code is visible only to the player
//! who ran the command. Only the bot token authenticates the bot. It is passed
//! straight to the client and shard and is never logged.

use std::collections::VecDeque;
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use infrarust_api::command::{CommandDispatchOutcome, CommandManager, CommandSource};
use infrarust_api::permissions::{PermissionChecker, PermissionMap};
use infrarust_api::services::PlayerRegistry;
use infrarust_api::services::ban_service::{
    BanRequest, BanService, BanSource, BanTarget, UnbanRequest,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use twilight_gateway::{CloseFrame, Event, EventTypeFlags, Intents, Shard, ShardId, StreamExt};
use twilight_http::Client;
use twilight_http::api_error::ApiError;
use twilight_http::error::ErrorType;
use twilight_model::application::command::Command;
use twilight_model::application::interaction::application_command::{
    CommandData, CommandOptionValue,
};
use twilight_model::application::interaction::{Interaction, InteractionData};
use twilight_model::channel::message::MessageFlags;
use twilight_model::gateway::payload::incoming::GuildCreate;
use twilight_model::http::interaction::{
    InteractionResponse, InteractionResponseData, InteractionResponseType,
};
use twilight_model::id::Id;
use twilight_model::id::marker::{ApplicationMarker, UserMarker};

use crate::config::{ConsoleConfig, MessageTemplates, ModerationConfig, NicknameConfig};
use crate::limits::SlidingWindow;
use crate::relay::ChatBridge;
use crate::store::{LinkBook, LinkError};
use crate::voice::VoiceProximity;

const BAN_SYNC_SOURCE: &str = "discord-sync";
const BAN_COMMAND_SOURCE: &str = "discord-command";
const CONSOLE_OUTPUT_LIMIT: usize = 1800;
/// Discord's "Unknown Ban" error: the user has no ban in that guild.
const UNKNOWN_BAN: u64 = 10026;
/// Spacing between per-user ban reconciliations after a READY event. Work is
/// paced on the gateway loop so heartbeats are never starved.
const BAN_RECONCILE_INTERVAL: Duration = Duration::from_millis(250);
/// Discord rejects message content longer than this many characters.
const REPLY_CHAR_LIMIT: usize = 2000;

pub struct Bot {
    token: String,
    http: Arc<Client>,
    application_id: Option<NonZeroU64>,
    links: Arc<LinkBook>,
    failures: Arc<SlidingWindow>,
    bridge: Arc<ChatBridge>,
    bans: Arc<dyn BanService>,
    moderation: ModerationConfig,
    nicknames: NicknameConfig,
    players: Arc<dyn PlayerRegistry>,
    commands: Arc<dyn CommandManager>,
    console: ConsoleConfig,
    messages: MessageTemplates,
    voice: Option<Arc<VoiceProximity>>,
}

impl Bot {
    pub(crate) fn new(
        token: String,
        http: Arc<Client>,
        application_id: Option<u64>,
        links: Arc<LinkBook>,
        failures: Arc<SlidingWindow>,
        bridge: Arc<ChatBridge>,
        bans: Arc<dyn BanService>,
        moderation: ModerationConfig,
        nicknames: NicknameConfig,
        players: Arc<dyn PlayerRegistry>,
        commands: Arc<dyn CommandManager>,
        console: ConsoleConfig,
        messages: MessageTemplates,
        voice: Option<Arc<VoiceProximity>>,
    ) -> Self {
        Self {
            token,
            http,
            application_id: application_id.and_then(NonZeroU64::new),
            links,
            failures,
            bridge,
            bans,
            moderation,
            nicknames,
            players,
            commands,
            console,
            messages,
            voice,
        }
    }

    /// Runs until `cancel` fires. Each failure is logged and the loop keeps
    /// running. The gate fails closed while the gateway is unreachable.
    pub async fn run(self, cancel: CancellationToken) {
        let application_id = tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!("Discord bot stopped before startup completed");
                return;
            }
            result = self.resolve_application_id() => match result {
                Ok(id) => id,
                Err(err) => {
                    tracing::error!(error = %err, "Discord bot could not resolve its application ID");
                    return;
                }
            },
        };
        tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!("Discord bot stopped before startup completed");
                return;
            }
            result = self.register_commands(application_id) => {
                if let Err(err) = result {
                    tracing::error!(error = %err, "Discord bot could not register slash commands");
                }
            }
        }

        let mut intents = Intents::GUILDS | Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT;
        if self.moderation.sync_bans && !self.moderation.guild_ids.is_empty() {
            intents |= Intents::GUILD_MODERATION;
        }
        if self.voice.is_some() {
            intents |= Intents::GUILD_VOICE_STATES;
        }
        let mut shard = Shard::new(ShardId::ONE, self.token.clone(), intents);
        tracing::info!("Discord bot gateway started");
        let voice_interval = self.voice.as_ref().map(|_| Duration::from_secs(5));
        let mut voice_tick =
            tokio::time::interval(voice_interval.unwrap_or(Duration::from_secs(3600)));
        let mut ban_reconcile_tick = tokio::time::interval(BAN_RECONCILE_INTERVAL);
        // Linked Discord users still to check against guild bans. A READY event
        // refills it, which covers ban events missed while the gateway was down.
        let mut pending_ban_checks: VecDeque<u64> = VecDeque::new();
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = voice_tick.tick(), if self.voice.is_some() => {
                    if let Some(voice) = &self.voice { voice.reconcile().await; }
                }
                _ = ban_reconcile_tick.tick(), if !pending_ban_checks.is_empty() => {
                    if let Some(user_id) = pending_ban_checks.pop_front() {
                        self.reconcile_user_bans(user_id).await;
                    }
                }
                next = shard.next_event(EventTypeFlags::INTERACTION_CREATE | EventTypeFlags::MESSAGE_CREATE | EventTypeFlags::BAN_ADD | EventTypeFlags::BAN_REMOVE | EventTypeFlags::READY | if self.voice.is_some() { EventTypeFlags::GUILD_VOICE_STATES | EventTypeFlags::GUILD_CREATE } else { EventTypeFlags::empty() }) => match next {
                    Some(Ok(Event::InteractionCreate(interaction))) => {
                        tokio::select! {
                            () = cancel.cancelled() => break,
                            () = self.handle_interaction(application_id, &interaction.0) => {}
                        }
                    }
                    Some(Ok(Event::BanAdd(event))) if self.moderation.sync_bans => {
                        self.import_ban(event.guild_id.get(), event.user.id.get()).await;
                    }
                    Some(Ok(Event::BanRemove(event))) if self.moderation.sync_bans => {
                        self.remove_imported_ban(event.guild_id.get(), event.user.id.get()).await;
                    }
                    Some(Ok(Event::Ready(_)))
                        if self.moderation.sync_bans && !self.moderation.guild_ids.is_empty() =>
                    {
                        // Reconnects can drop ban events, so every linked user is re-checked.
                        pending_ban_checks = self
                            .links
                            .linked_discord_ids()
                            .await
                            .iter()
                            .filter_map(|id| id.parse::<u64>().ok())
                            .collect();
                        tracing::info!(
                            users = pending_ban_checks.len(),
                            "Discord gateway ready; reconciling linked users against guild bans"
                        );
                    }
                    Some(Ok(Event::MessageCreate(message))) => {
                        tokio::select! {
                            () = cancel.cancelled() => break,
                            () = self.bridge.handle_discord_message(&message.0) => {}
                        }
                    }
                    Some(Ok(Event::VoiceStateUpdate(event))) => {
                        if let Some(voice) = &self.voice
                            && let Some(guild_id) = event.0.guild_id
                            && voice.observes_guild(guild_id.get())
                        {
                            voice.update_state(
                                guild_id.get(),
                                event.0.user_id.get(),
                                event.0.channel_id.map(|channel| channel.get()),
                            ).await;
                        }
                    }
                    Some(Ok(Event::GuildCreate(event))) => {
                        if let Some(voice) = &self.voice
                            && let GuildCreate::Available(guild) = event.as_ref()
                            && voice.observes_guild(guild.id.get())
                        {
                            for state in &guild.voice_states {
                                voice.track_state(
                                    guild.id.get(),
                                    state.user_id.get(),
                                    state.channel_id.map(|channel| channel.get()),
                                );
                            }
                            voice.reconcile().await;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(err)) => {
                        tracing::warn!(error = %err, "Discord gateway receive failed");
                        tokio::select! {
                            () = cancel.cancelled() => break,
                            () = tokio::time::sleep(Duration::from_secs(1)) => {}
                        }
                    }
                    None => break,
                },
            }
        }
        shard.close(CloseFrame::NORMAL);
        tracing::info!("Discord bot gateway stopped");
    }

    async fn import_ban(&self, guild_id: u64, user_id: u64) {
        if !self
            .moderation
            .guild_ids
            .iter()
            .any(|id| id.trim() == guild_id.to_string())
        {
            return;
        }
        let Some(uuid) = self.links.linked_uuid(&user_id.to_string()).await else {
            tracing::debug!(
                guild_id,
                user_id,
                "Discord ban has no linked Minecraft identity; skipped"
            );
            return;
        };
        let target = BanTarget::Uuid(uuid);
        match self.bans.get(&target).await {
            Ok(Some(existing)) if existing.source == BanSource::Plugin(BAN_SYNC_SOURCE.into()) => {}
            Ok(Some(existing)) => {
                tracing::info!(%uuid, source = %existing.source, "Preserving an existing non-Discord ban");
                return;
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(%uuid, error = %err, "Could not check existing ban; preserving it");
                return;
            }
        }
        let request = BanRequest::new(BanTarget::Uuid(uuid))
            .reason("Discord guild ban")
            .source(BanSource::Plugin(BAN_SYNC_SOURCE.into()));
        if let Err(err) = self.bans.ban(request).await {
            tracing::warn!(%uuid, error = %err, "Could not import Discord ban");
        }
    }

    async fn remove_imported_ban(&self, guild_id: u64, user_id: u64) {
        if !self
            .moderation
            .guild_ids
            .iter()
            .any(|id| id.trim() == guild_id.to_string())
        {
            return;
        }
        let Some(uuid) = self.links.linked_uuid(&user_id.to_string()).await else {
            return;
        };
        let target = BanTarget::Uuid(uuid);
        match self.bans.get(&target).await {
            Ok(Some(entry)) if entry.source == BanSource::Plugin(BAN_SYNC_SOURCE.into()) => {}
            Ok(_) => return,
            Err(err) => {
                tracing::warn!(%uuid, error = %err, "Could not identify imported ban source; preserving proxy ban");
                return;
            }
        }
        // Keep the proxy ban while the linked user remains banned in any other
        // allowlisted guild. Failed checks also preserve the ban.
        for raw in &self.moderation.guild_ids {
            let Some(guild) = crate::membership::parse_guild(raw) else {
                tracing::warn!(guild = %raw, "invalid moderation guild ID; preserving proxy ban");
                return;
            };
            if guild.get() == guild_id {
                continue;
            }
            match self.http.ban(guild, Id::new(user_id)).await {
                Ok(_) => return,
                Err(err) if matches!(err.kind(), ErrorType::Response { status, .. } if status.get() == 404) =>
                    {}
                Err(err) => {
                    tracing::warn!(%uuid, guild = %guild, error = %err, "Could not verify other Discord guild bans; preserving proxy ban");
                    return;
                }
            }
        }
        let request = UnbanRequest::new(target).source(BanSource::Plugin(BAN_SYNC_SOURCE.into()));
        if let Err(err) = self.bans.unban(request).await {
            tracing::warn!(%uuid, error = %err, "Could not remove imported Discord ban");
        }
    }

    /// Makes the proxy's Discord-sourced ban match the guild bans for one linked
    /// user. A lookup error in any guild preserves the current proxy ban. Bans from
    /// other sources are never changed.
    async fn reconcile_user_bans(&self, user_id: u64) {
        let Some(uuid) = self.links.linked_uuid(&user_id.to_string()).await else {
            return;
        };
        let mut banned_guild = None;
        for raw in &self.moderation.guild_ids {
            let Some(guild) = crate::membership::parse_guild(raw) else {
                tracing::warn!(guild = %raw, "invalid moderation guild ID; preserving proxy ban");
                return;
            };
            match self.http.ban(guild, Id::new(user_id)).await {
                Ok(_) => banned_guild = Some(guild.get()),
                Err(err) if is_unknown_ban(&err) => {}
                Err(err) => {
                    tracing::warn!(guild = %guild, error = %err, "Could not reconcile Discord guild bans; preserving proxy ban");
                    return;
                }
            }
        }
        match banned_guild {
            Some(guild_id) => self.import_ban(guild_id, user_id).await,
            None => self.remove_discord_sync_ban(uuid).await,
        }
    }

    async fn remove_discord_sync_ban(&self, uuid: uuid::Uuid) {
        let target = BanTarget::Uuid(uuid);
        match self.bans.get(&target).await {
            Ok(Some(entry)) if entry.source == BanSource::Plugin(BAN_SYNC_SOURCE.into()) => {}
            Ok(_) => return,
            Err(err) => {
                tracing::warn!(error = %err, "Could not identify imported ban source; preserving proxy ban");
                return;
            }
        }
        let request = UnbanRequest::new(target).source(BanSource::Plugin(BAN_SYNC_SOURCE.into()));
        if let Err(err) = self.bans.unban(request).await {
            tracing::warn!(error = %err, "Could not remove imported Discord ban");
        }
    }

    async fn reconcile_linked_bans(&self, user_id: u64) {
        for raw in &self.moderation.guild_ids {
            let Some(guild) = crate::membership::parse_guild(raw) else {
                tracing::warn!(guild = %raw, "invalid moderation guild ID; skipped link-time ban reconciliation");
                continue;
            };
            match self.http.ban(guild, Id::new(user_id)).await {
                Ok(_) => self.import_ban(guild.get(), user_id).await,
                Err(err) if matches!(err.kind(), ErrorType::Response { status, .. } if status.get() == 404) =>
                    {}
                Err(err) => {
                    tracing::warn!(guild = %guild, user_id, error = %err, "Could not reconcile Discord bans for newly linked account")
                }
            }
        }
    }

    async fn resolve_application_id(&self) -> Result<Id<ApplicationMarker>, String> {
        if let Some(id) = self.application_id {
            return Ok(Id::from(id));
        }
        let application = self
            .http
            .current_user_application()
            .await
            .map_err(|err| err.to_string())?
            .model()
            .await
            .map_err(|err| err.to_string())?;
        Ok(application.id)
    }

    async fn register_commands(&self, application_id: Id<ApplicationMarker>) -> Result<(), String> {
        let commands = command_definitions().map_err(|err| err.to_string())?;
        self.http
            .interaction(application_id)
            .set_global_commands(&commands)
            .await
            .map_err(|err| err.to_string())?;
        Ok(())
    }

    async fn handle_interaction(
        &self,
        application_id: Id<ApplicationMarker>,
        interaction: &Interaction,
    ) {
        let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
            return;
        };
        let Some(user) = interaction.author_id() else {
            return;
        };

        if data.name == "console" {
            let deferred = InteractionResponse {
                kind: InteractionResponseType::DeferredChannelMessageWithSource,
                data: Some(InteractionResponseData {
                    flags: Some(MessageFlags::EPHEMERAL),
                    ..Default::default()
                }),
            };
            if let Err(err) = self
                .http
                .interaction(application_id)
                .create_response(interaction.id, &interaction.token, &deferred)
                .await
            {
                tracing::warn!(error = %err, "Failed to acknowledge Discord console interaction");
                return;
            }
            let reply = self
                .console_command(interaction, user, option_string(data, "command"))
                .await;
            let reply = bounded_reply(reply);
            if let Err(err) = self
                .http
                .interaction(application_id)
                .create_followup(&interaction.token)
                .content(&reply)
                .flags(MessageFlags::EPHEMERAL)
                .await
            {
                tracing::warn!(error = %err, "Failed to answer Discord console interaction");
            }
            return;
        }

        let reply = match data.name.as_str() {
            "link" => self.link(user, option_string(data, "code")).await,
            "unlink" => self.unlink(user).await,
            "status" | "players" => {
                if interaction.guild_id.is_some() {
                    if data.name == "status" {
                        self.status()
                    } else {
                        self.players()
                    }
                } else {
                    self.messages.command_server_only.clone()
                }
            }
            "whois" | "ban" | "unban" => {
                if !self.is_staff(interaction) {
                    self.messages.command_staff_only.clone()
                } else {
                    match data.name.as_str() {
                        "whois" => self.whois(option_string(data, "uuid")).await,
                        "ban" => self.ban(user, data).await,
                        "unban" => self.unban(data).await,
                        _ => self.messages.command_unknown.clone(),
                    }
                }
            }
            _ => self.messages.command_unknown.clone(),
        };

        let response = InteractionResponse {
            kind: InteractionResponseType::ChannelMessageWithSource,
            data: Some(InteractionResponseData {
                content: Some(bounded_reply(reply)),
                flags: Some(MessageFlags::EPHEMERAL),
                ..Default::default()
            }),
        };
        if let Err(err) = self
            .http
            .interaction(application_id)
            .create_response(interaction.id, &interaction.token, &response)
            .await
        {
            tracing::warn!(error = %err, "Failed to answer a Discord interaction");
        }
    }

    async fn link(&self, user: Id<UserMarker>, code: Option<String>) -> String {
        let discord_id = user.get().to_string();
        if !self.failures.allows(&discord_id) {
            return self.messages.link_rate_limited.clone();
        }
        let Some(code) = code else {
            return self.messages.link_usage.clone();
        };

        match self.links.redeem(&code, &discord_id).await {
            Ok(uuid) => {
                tracing::info!(%uuid, "Linked a Discord account");
                self.sync_nickname(uuid, user).await;
                if self.moderation.sync_bans {
                    self.reconcile_linked_bans(user.get()).await;
                }
                self.messages.link_success.clone()
            }
            Err(LinkError::InvalidCode) => {
                self.failures.record(&discord_id);
                self.messages.link_invalid_code.clone()
            }
            Err(LinkError::DiscordAlreadyLinked) => self.messages.link_discord_taken.clone(),
            Err(LinkError::MinecraftAlreadyLinked) => self.messages.link_minecraft_taken.clone(),
            Err(err) => {
                tracing::error!(error = %err, "Failed to save a Discord link");
                self.messages.link_failed.clone()
            }
        }
    }

    fn is_staff(&self, interaction: &Interaction) -> bool {
        let Some(member) = &interaction.member else {
            return false;
        };
        if self.moderation.staff_role_ids.is_empty() || interaction.guild_id.is_none() {
            return false;
        }
        member.roles.iter().any(|role| {
            self.moderation
                .staff_role_ids
                .iter()
                .any(|allowed| allowed.trim() == role.get().to_string())
        })
    }

    fn status(&self) -> String {
        render(
            &self.messages.status_online,
            &[("count", self.players.online_count().to_string())],
        )
    }

    fn players(&self) -> String {
        const LISTED: usize = 40;
        let players = self.players.get_all_players();
        if players.is_empty() {
            return self.messages.players_none.clone();
        }
        let mut names: Vec<_> = players
            .iter()
            .map(|player| neutralize(&player.profile().username))
            .collect();
        names.sort_by_key(|name| name.to_lowercase());
        let shown = names
            .iter()
            .take(LISTED)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        if names.len() > LISTED {
            render(
                &self.messages.players_list_truncated,
                &[
                    ("count", names.len().to_string()),
                    ("players", shown),
                    ("more", (names.len() - LISTED).to_string()),
                ],
            )
        } else {
            render(
                &self.messages.players_list,
                &[("count", names.len().to_string()), ("players", shown)],
            )
        }
    }

    async fn whois(&self, uuid: Option<String>) -> String {
        let Some(uuid) = uuid.and_then(|value| value.parse::<uuid::Uuid>().ok()) else {
            return self.messages.whois_invalid_uuid.clone();
        };
        let Some(player) = self.players.get_player_by_uuid(&uuid) else {
            return render(&self.messages.whois_offline, &[("uuid", uuid.to_string())]);
        };
        let discord = match self.links.linked_discord(uuid).await {
            Some(id) => neutralize(&id),
            None => self.messages.whois_not_linked.clone(),
        };
        render(
            &self.messages.whois_result,
            &[
                ("username", neutralize(&player.profile().username)),
                ("uuid", uuid.to_string()),
                ("discord", discord),
            ],
        )
    }

    async fn ban(&self, actor: Id<UserMarker>, data: &CommandData) -> String {
        let Some(uuid) =
            option_string(data, "uuid").and_then(|value| value.parse::<uuid::Uuid>().ok())
        else {
            return self.messages.ban_invalid_uuid.clone();
        };
        let reason = option_string(data, "reason").unwrap_or_else(|| "No reason provided".into());
        let duration = match option_string(data, "duration_seconds") {
            Some(value) => match value.parse::<u64>() {
                Ok(seconds) if (1..=31_536_000).contains(&seconds) => {
                    Some(Duration::from_secs(seconds))
                }
                _ => return self.messages.ban_invalid_duration.clone(),
            },
            None => None,
        };
        let target = BanTarget::Uuid(uuid);
        match self.bans.get(&target).await {
            Ok(Some(existing))
                if existing.source != BanSource::Plugin(BAN_COMMAND_SOURCE.into()) =>
            {
                return render(
                    &self.messages.ban_existing,
                    &[
                        ("uuid", uuid.to_string()),
                        ("source", neutralize(&existing.source.to_string())),
                    ],
                );
            }
            Err(err) => {
                return render(
                    &self.messages.ban_lookup_failed,
                    &[("error", neutralize(&err.to_string()))],
                );
            }
            _ => {}
        }
        let mut request = BanRequest::new(BanTarget::Uuid(uuid))
            .reason(format!("{reason} (Discord staff {})", actor.get()))
            .source(BanSource::Plugin(BAN_COMMAND_SOURCE.into()));
        if let Some(duration) = duration {
            request = request.duration(duration);
        }
        match self.bans.ban(request).await {
            Ok(entry) => {
                let expires = entry
                    .expires_at
                    .map(|at| {
                        at.duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs()
                            .to_string()
                    })
                    .unwrap_or_else(|| self.messages.ban_expires_never.clone());
                render(
                    &self.messages.ban_success,
                    &[("uuid", uuid.to_string()), ("expires", expires)],
                )
            }
            Err(err) => render(
                &self.messages.ban_failed,
                &[
                    ("uuid", uuid.to_string()),
                    ("error", neutralize(&err.to_string())),
                ],
            ),
        }
    }

    async fn unban(&self, data: &CommandData) -> String {
        let Some(uuid) =
            option_string(data, "uuid").and_then(|value| value.parse::<uuid::Uuid>().ok())
        else {
            return self.messages.unban_invalid_uuid.clone();
        };
        match self
            .bans
            .unban(
                UnbanRequest::new(BanTarget::Uuid(uuid))
                    .source(BanSource::Plugin(BAN_COMMAND_SOURCE.into())),
            )
            .await
        {
            Ok(Some(_)) => render(&self.messages.unban_success, &[("uuid", uuid.to_string())]),
            Ok(None) => render(
                &self.messages.unban_not_found,
                &[("uuid", uuid.to_string())],
            ),
            Err(err) => render(
                &self.messages.unban_failed,
                &[
                    ("uuid", uuid.to_string()),
                    ("error", neutralize(&err.to_string())),
                ],
            ),
        }
    }

    async fn console_command(
        &self,
        interaction: &Interaction,
        user: Id<UserMarker>,
        line: Option<String>,
    ) -> String {
        if !self.console.enabled {
            return self.messages.console_disabled.clone();
        }
        let Some(channel) = interaction.channel.as_ref().map(|channel| channel.id) else {
            return self.messages.console_channel_required.clone();
        };
        if !self
            .console
            .allowed_channel_ids
            .iter()
            .any(|allowed| allowed.trim() == channel.get().to_string())
        {
            return self.messages.console_channel_denied.clone();
        }
        let user_allowed = self
            .console
            .allowed_user_ids
            .iter()
            .any(|allowed| allowed.trim() == user.get().to_string());
        let role_allowed = interaction.member.as_ref().is_some_and(|member| {
            member.roles.iter().any(|role| {
                self.console
                    .allowed_role_ids
                    .iter()
                    .any(|allowed| allowed.trim() == role.get().to_string())
            })
        });
        if !user_allowed && !role_allowed {
            return self.messages.console_user_denied.clone();
        }
        let Some(line) = line else {
            return self.messages.console_line_required.clone();
        };
        if line.is_empty()
            || line.starts_with('/')
            || line.trim() != line
            || line.len() > 100
            || line.chars().any(char::is_control)
        {
            return self.messages.console_line_invalid.clone();
        }
        let Some(permission) = self
            .console
            .commands
            .get(&line)
            .filter(|node| valid_console_node(node))
        else {
            return self.messages.console_not_allowlisted.clone();
        };

        let output = Arc::new(Mutex::new(String::new()));
        let output_sink = Arc::clone(&output);
        let checker: Arc<dyn PermissionChecker> =
            Arc::new(PermissionMap::new().with(permission, true));
        let source = CommandSource::remote(
            format!("Discord:{}", user.get()),
            checker,
            move |component| {
                let text = component.to_plain();
                let mut output = output_sink.lock().unwrap_or_else(PoisonError::into_inner);
                append_console_output(&mut output, &text);
            },
        );
        match self.commands.dispatch(source, &line).await {
            CommandDispatchOutcome::Executed => {
                let output = output.lock().unwrap_or_else(PoisonError::into_inner);
                if output.is_empty() {
                    self.messages.console_no_output.clone()
                } else {
                    output.clone()
                }
            }
            CommandDispatchOutcome::Denied => self.messages.console_denied.clone(),
            CommandDispatchOutcome::Unknown => self.messages.console_unknown.clone(),
            _ => self.messages.console_unsupported.clone(),
        }
    }

    async fn sync_nickname(&self, uuid: uuid::Uuid, user: Id<UserMarker>) {
        if !self.nicknames.enabled {
            return;
        }
        let Some(player) = self.players.get_player_by_uuid(&uuid) else {
            return;
        };
        sync_nickname_to_guilds(
            Arc::clone(&self.http),
            user,
            self.nicknames.clone(),
            uuid,
            player.profile().username.clone(),
        )
        .await;
    }

    async fn unlink(&self, user: Id<UserMarker>) -> String {
        let discord_id = user.get().to_string();
        match self.links.unlink(&discord_id).await {
            Ok(Some(uuid)) => {
                tracing::info!(%uuid, "Removed a Discord link");
                self.messages.unlink_success.clone()
            }
            Ok(None) => self.messages.unlink_not_linked.clone(),
            Err(err) => {
                tracing::error!(error = %err, "Failed to remove a Discord link");
                self.messages.unlink_failed.clone()
            }
        }
    }
}

/// Only Discord's "Unknown Ban" 404 means "not banned". Any other failure, such
/// as an unknown guild, must be treated as unknown so the ban is preserved.
fn is_unknown_ban(err: &twilight_http::Error) -> bool {
    matches!(
        err.kind(),
        ErrorType::Response {
            error: ApiError::General(general),
            status,
            ..
        } if status.get() == 404 && general.code == UNKNOWN_BAN
    )
}

fn valid_console_node(node: &str) -> bool {
    let node = node.trim();
    !node.is_empty()
        && !node.eq_ignore_ascii_case("infrarust.admin")
        && !node.to_ascii_lowercase().starts_with("infrarust.admin.")
        && node.len() <= 128
        && node.split('.').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
}

fn append_console_output(output: &mut String, message: &str) {
    if output.encode_utf16().count() >= CONSOLE_OUTPUT_LIMIT {
        return;
    }
    if !output.is_empty() {
        output.push('\n');
    }
    for original in message.chars() {
        let rendered = neutralize(&original.to_string());
        if output.encode_utf16().count() + rendered.encode_utf16().count() > CONSOLE_OUTPUT_LIMIT {
            break;
        }
        output.push_str(&rendered);
    }
}

/// Makes user-controlled text safe to show in a reply. Mentions are broken with
/// a zero-width space, Markdown and mention syntax is backslash-escaped, and
/// control characters other than newline and tab become U+FFFD.
fn neutralize(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '@' => out.push_str("@\u{200b}"),
            '\\' | '*' | '_' | '~' | '|' | '`' | '<' | '>' => {
                out.push('\\');
                out.push(character);
            }
            '\n' | '\t' => out.push(character),
            c if c.is_control() => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
    out
}

/// Fills `{name}` placeholders in one pass, so a substituted value is never
/// scanned again. Unknown placeholders are kept as written.
fn render(template: &str, values: &[(&str, String)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.find('}') else {
            break;
        };
        let name = &rest[1..end];
        match values.iter().find(|(key, _)| *key == name) {
            Some((_, value)) => out.push_str(value),
            None => out.push_str(&rest[..=end]),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Keeps a reply within Discord's message length limit.
fn bounded_reply(reply: String) -> String {
    if reply.chars().count() <= REPLY_CHAR_LIMIT {
        return reply;
    }
    reply.chars().take(REPLY_CHAR_LIMIT).collect()
}

pub async fn sync_nickname_to_guilds(
    http: Arc<Client>,
    user: Id<UserMarker>,
    config: NicknameConfig,
    uuid: uuid::Uuid,
    username: String,
) {
    if !config.enabled {
        return;
    }
    for raw in &config.guild_ids {
        let Some(guild) = crate::membership::parse_guild(raw) else {
            tracing::warn!(guild = %raw, "invalid nickname sync guild ID");
            continue;
        };
        if let Err(err) = http
            .update_guild_member(guild, user)
            .nick(Some(&username))
            .await
        {
            tracing::warn!(%uuid, guild = %guild, error = %err, "Could not sync linked Minecraft nickname to Discord");
        }
    }
}

/// Slash command definitions, deserialized from the Discord wire shape.
fn command_definitions() -> Result<Vec<Command>, serde_json::Error> {
    // `version` is a required field of the model. Discord assigns the real value.
    serde_json::from_value(json!([
        {
            "name": "link",
            "description": "Link your Discord account to your Minecraft account",
            "type": 1,
            "version": "1",
            "integration_types": [0],
            "contexts": [0, 1],
            "options": [{
                "type": 3,
                "name": "code",
                "description": "The code shown when you tried to join the server",
                "required": true
            }]
        },
        {
            "name": "unlink",
            "description": "Remove the Minecraft link from your Discord account",
            "type": 1,
            "version": "1",
            "integration_types": [0],
            "contexts": [0, 1],
            "options": []
        },
        {
            "name": "console",
            "description": "Run an exact proxy command from the configured allowlist",
            "type": 1,
            "version": "1",
            "integration_types": [0],
            "contexts": [0],
            "options": [{"type": 3, "name": "command", "description": "Exact allowlisted command line", "required": true, "max_length": 100}]
        },
        {
            "name": "status",
            "description": "Show proxy status",
            "type": 1,
            "version": "1",
            "integration_types": [0],
            "contexts": [0],
            "options": []
        },
        {
            "name": "players",
            "description": "List online Minecraft players",
            "type": 1,
            "version": "1",
            "integration_types": [0],
            "contexts": [0],
            "options": []
        },
        {
            "name": "whois",
            "description": "Look up an online Minecraft player by UUID",
            "type": 1,
            "version": "1",
            "integration_types": [0],
            "contexts": [0],
            "options": [{"type": 3, "name": "uuid", "description": "Minecraft UUID", "required": true}]
        },
        {
            "name": "ban",
            "description": "Ban a Minecraft UUID",
            "type": 1,
            "version": "1",
            "integration_types": [0],
            "contexts": [0],
            "options": [
                {"type": 3, "name": "uuid", "description": "Minecraft UUID (no IP lookup is performed)", "required": true},
                {"type": 3, "name": "reason", "description": "Reason for the ban", "required": true},
                {"type": 3, "name": "duration_seconds", "description": "Optional duration in seconds; omit for permanent", "required": false}
            ]
        },
        {
            "name": "unban",
            "description": "Unban a Minecraft UUID",
            "type": 1,
            "version": "1",
            "integration_types": [0],
            "contexts": [0],
            "options": [{"type": 3, "name": "uuid", "description": "Minecraft UUID", "required": true}]
        }
    ]))
}

fn option_string(data: &CommandData, name: &str) -> Option<String> {
    data.options
        .iter()
        .find(|option| option.name == name)
        .and_then(|option| match &option.value {
            CommandOptionValue::String(value) => Some(value.clone()),
            _ => None,
        })
}
