//! Discord account-linking stage shown before a protected backend is joined.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use infrarust_api::event::BoxFuture;
use infrarust_api::limbo::context::LimboEntryContext;
use infrarust_api::limbo::handler::{HandlerResult, LimboHandler, LimboOutcome};
use infrarust_api::limbo::session::LimboSession;
use infrarust_api::types::{Component, TitleData};
use uuid::Uuid;

use crate::config::GateConfig;
use crate::store::{LinkBook, LinkError};

const REMINDER_INTERVAL: Duration = Duration::from_secs(1);
const DISCORD_COLOR: &str = "#5865F2";
const DISCORD_LINK_LIMBO_HANDLER: &str = "discord-link";

pub struct DiscordLinkHandler {
    links: Arc<LinkBook>,
    bot_available: bool,
    bypass_uuids: HashSet<Uuid>,
}

impl DiscordLinkHandler {
    pub fn new(links: Arc<LinkBook>, bot_available: bool, config: &GateConfig) -> Self {
        Self {
            links,
            bot_available,
            bypass_uuids: config.bypass_uuids.iter().copied().collect(),
        }
    }
}

impl LimboHandler for DiscordLinkHandler {
    fn name(&self) -> &str {
        DISCORD_LINK_LIMBO_HANDLER
    }

    fn on_player_enter<'a>(
        &'a self,
        session: &'a dyn LimboSession,
    ) -> BoxFuture<'a, HandlerResult> {
        Box::pin(async move {
            // The mutable display profile is not an authentication credential.
            // Only core can supply this context from its preserved Mojang UUID.
            let LimboEntryContext::AdmissionGuard {
                namespace,
                handler,
                authenticated_uuid,
                ..
            } = session.entry_context()
            else {
                return HandlerResult::Deny(Component::error(
                    "Discord linking is unavailable for this connection.",
                ));
            };
            if namespace != "discord" || handler != DISCORD_LINK_LIMBO_HANDLER {
                return HandlerResult::Deny(Component::error(
                    "Discord linking is unavailable for this connection.",
                ));
            }
            let uuid = *authenticated_uuid;
            if self.bypass_uuids.contains(&uuid) {
                return HandlerResult::Accept;
            }
            if !self.bot_available {
                return HandlerResult::Deny(Component::error(
                    "Discord verification is unavailable right now. Please try again later.",
                ));
            }
            if self.links.linked_discord(uuid).await.is_some() {
                show_verifying(session);
                return HandlerResult::Accept;
            }
            let (code, expires_at) = match self.links.pending_code(uuid).await {
                Ok(pending) => pending,
                // Redemption can finish after the gate requested limbo but
                // before this handler starts. Core will verify membership next.
                Err(LinkError::MinecraftAlreadyLinked) => {
                    show_verifying(session);
                    return HandlerResult::Accept;
                }
                Err(LinkError::TooManyPending) => {
                    return HandlerResult::Deny(Component::error(
                        "Too many players are linking right now. Please try again shortly.",
                    ));
                }
                Err(err) => {
                    tracing::warn!(%uuid, error = %err, "could not prepare Discord linking limbo");
                    return HandlerResult::Deny(Component::error(
                        "Discord linking is unavailable right now. Please try again shortly.",
                    ));
                }
            };
            let remaining = expires_at.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return HandlerResult::Deny(expired_message());
            }

            let _ = session.send_title(link_title(&code));
            let _ = session.send_action_bar(countdown(expires_at));

            let links = Arc::clone(&self.links);
            let handle = session.handle();
            let cancel = session.cancellation_token();
            tokio::spawn(async move {
                let linked = links.wait_until_linked(uuid, &code);
                tokio::pin!(linked);
                let mut reminder = tokio::time::interval(REMINDER_INTERVAL);
                reminder.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                // The first prompt was queued above, so skip the immediate tick.
                reminder.tick().await;
                let mut title_ticks = 0_u8;
                loop {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => break,
                        linked = &mut linked => {
                            if !linked {
                                handle.complete(LimboOutcome::Deny(Component::error(
                                    "Your Discord link changed. Rejoin to continue linking.",
                                )));
                                break;
                            }
                            let _ = handle.send_title(
                                TitleData::new(
                                    Component::text("Linked").color("green").bold(),
                                    Component::text("Checking Discord membership...").color("gray"),
                                )
                                .fade_in(0)
                                .stay(60)
                                .fade_out(10),
                            );
                            let _ = handle.send_action_bar(Component::text(""));
                            handle.complete(LimboOutcome::Accept);
                            break;
                        }
                        _ = reminder.tick() => {
                            title_ticks += 1;
                            if title_ticks == 5 {
                                let _ = handle.send_title(link_title(&code));
                                title_ticks = 0;
                            }
                            let _ = handle.send_action_bar(countdown(expires_at));
                        }
                    }
                }
            });

            // The engine owns the deadline even if the background task fails.
            HandlerResult::HoldWithTimeout {
                after: remaining,
                on_timeout: LimboOutcome::Deny(expired_message()),
            }
        })
    }

    fn allows_proxy_commands(&self) -> bool {
        false
    }
}

fn link_title(code: &str) -> TitleData {
    TitleData::new(
        Component::text(code).color(DISCORD_COLOR).bold(),
        Component::text("Enter /link in Discord to join").color("white"),
    )
    .fade_in(0)
    .stay(120)
    .fade_out(10)
}

fn show_verifying(session: &dyn LimboSession) {
    let _ = session.send_title(
        TitleData::new(
            Component::text("Verifying").color(DISCORD_COLOR).bold(),
            Component::text("Checking Discord membership...").color("gray"),
        )
        .fade_in(0)
        .stay(200)
        .fade_out(10),
    );
}

/// Status-only line: the code and the `/link` instruction live in the title,
/// so the action bar does not repeat them. It turns yellow in the final minute.
fn countdown(expires_at: Instant) -> Component {
    let seconds = expires_at
        .saturating_duration_since(Instant::now())
        .as_secs();
    let color = if seconds <= 60 { "yellow" } else { "gray" };
    Component::text(format!(
        "Code expires in {}:{:02}",
        seconds / 60,
        seconds % 60,
    ))
    .color(color)
}

fn expired_message() -> Component {
    Component::text("Your Discord link code expired. Rejoin to get a new code.").color("yellow")
}
