//! The Discord community gate, bound to the core `DiscordAuthorizer` slot.
//!
//! Decision order for a player the proxy has already verified with Mojang:
//! 1. no link: prepare a link code and request the Discord limbo stage.
//!    An existing live code is reused.
//! 2. linked: allow on the first confirmed membership in any listed guild.
//! 3. linked, and every guild answered NotMember: deny without a code, since
//!    a new code cannot fix a missing membership.
//! 4. any lookup failed without a confirmed member: `ServiceError::Unavailable`,
//!    which the core treats as a refusal. It is never an allow.

use std::collections::HashSet;
use std::num::NonZeroU64;
use std::sync::Arc;

use infrarust_api::error::ServiceError;
use infrarust_api::event::BoxFuture;
use infrarust_api::services::{
    AdmissionDecision, AdmissionGuard, AdmissionRequest, AdmissionRequirements,
};
use uuid::Uuid;

use crate::config::GateConfig;
use crate::config::RoleGateConfig;
use crate::membership::{Membership, MembershipChecker, parse_guild, parse_user};
use crate::store::{LinkBook, LinkError};

pub struct DiscordGate {
    links: Arc<LinkBook>,
    membership: Option<Arc<MembershipChecker>>,
    bypass_uuids: HashSet<Uuid>,
    role_gate: RoleGateConfig,
}

impl DiscordGate {
    pub fn new(
        links: Arc<LinkBook>,
        membership: Option<Arc<MembershipChecker>>,
        config: &GateConfig,
        role_gate: &RoleGateConfig,
    ) -> Self {
        Self {
            links,
            membership,
            bypass_uuids: config.bypass_uuids.iter().copied().collect(),
            role_gate: role_gate.clone(),
        }
    }

    async fn decide(&self, request: AdmissionRequest) -> Result<AdmissionDecision, ServiceError> {
        if self.bypass_uuids.contains(&request.uuid) {
            return Ok(AdmissionDecision::Allow);
        }
        let Some(membership) = &self.membership else {
            return Err(ServiceError::Unavailable(
                "Discord bot is not configured".into(),
            ));
        };

        let guild_ids = request
            .policy
            .get("communities")
            .map(|value| {
                value.as_array().ok_or_else(|| {
                    ServiceError::OperationFailed(
                        "Discord admission policy `communities` must be an array".into(),
                    )
                })
            })
            .transpose()?
            .into_iter()
            .flatten()
            .map(|value| {
                value.as_str().ok_or_else(|| {
                    ServiceError::OperationFailed(
                        "Discord community IDs in admission policy must be strings".into(),
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let guilds = guild_ids
            .iter()
            .map(|raw| {
                parse_guild(raw).ok_or_else(|| {
                    ServiceError::OperationFailed(format!("invalid Discord community ID `{raw}`"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if guilds.is_empty() {
            return Ok(deny("This server has no Discord community configured."));
        }

        let Some(discord_id) = self.links.linked_discord(request.uuid).await else {
            return self.link_required(request.uuid).await;
        };
        let user = parse_user(&discord_id)
            .ok_or_else(|| ServiceError::OperationFailed("stored Discord ID is invalid".into()))?;

        let mut unavailable = false;
        let mut role_denied = false;
        for guild in guilds {
            match membership.check(user, guild).await {
                Ok(Membership::Member) => {
                    let raw_roles = self.role_gate.guild_roles.get(&guild.get().to_string());
                    let Some(raw_roles) = raw_roles.filter(|roles| !roles.is_empty()) else {
                        return Ok(AdmissionDecision::Allow);
                    };
                    let required = raw_roles
                        .iter()
                        .map(|raw| {
                            raw.trim()
                                .parse::<NonZeroU64>()
                                .map(NonZeroU64::get)
                                .map_err(|_| {
                                    ServiceError::OperationFailed(format!(
                                        "invalid Discord role ID `{raw}` in private config"
                                    ))
                                })
                        })
                        .collect::<Result<HashSet<_>, _>>()?;
                    match membership.roles(user, guild).await {
                        Ok(Some(roles)) if roles.iter().any(|role| required.contains(role)) => {
                            return Ok(AdmissionDecision::Allow);
                        }
                        Ok(Some(_)) => role_denied = true,
                        Ok(None) => {}
                        Err(err) => {
                            tracing::warn!(uuid = %request.uuid, guild = %guild, error = %err, "Discord role lookup failed");
                            unavailable = true;
                        }
                    }
                }
                Ok(Membership::NotMember) => {}
                Err(err) => {
                    tracing::warn!(
                        uuid = %request.uuid,
                        guild = %guild,
                        error = %err,
                        "Discord membership lookup failed"
                    );
                    unavailable = true;
                }
            }
        }

        if unavailable {
            return Err(ServiceError::Unavailable(
                "Discord membership could not be verified".into(),
            ));
        }
        Ok(deny(if role_denied {
            "You need an approved role in this server's Discord community to play here."
        } else {
            "Join one of this server's Discord communities to play here."
        }))
    }

    async fn link_required(&self, uuid: Uuid) -> Result<AdmissionDecision, ServiceError> {
        match self.links.pending_code(uuid).await {
            Ok(_) => Ok(AdmissionDecision::Hold {
                handler: "discord-link".to_owned(),
            }),
            Err(LinkError::TooManyPending) => Ok(deny(
                "Too many players are linking right now. Try again in a few minutes.",
            )),
            Err(LinkError::MinecraftAlreadyLinked) => Err(ServiceError::Unavailable(
                "the account was linked concurrently; retry".into(),
            )),
            Err(err) => Err(ServiceError::OperationFailed(err.to_string())),
        }
    }
}

impl AdmissionGuard for DiscordGate {
    fn requirements(&self) -> AdmissionRequirements {
        AdmissionRequirements::new(true, true)
    }

    fn authorize(
        &self,
        request: AdmissionRequest,
    ) -> BoxFuture<'_, Result<AdmissionDecision, ServiceError>> {
        Box::pin(async move { self.decide(request).await })
    }
}

fn deny(reason: impl Into<String>) -> AdmissionDecision {
    AdmissionDecision::Deny {
        reason: Some(reason.into()),
    }
}
