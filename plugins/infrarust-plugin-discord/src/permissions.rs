//! Optional Discord role to Minecraft permission-node mappings.

use std::num::NonZeroU64;
use std::sync::Arc;

use infrarust_api::permissions::{
    PermissionChecker, PermissionMap, PermissionProvider, PermissionSubject,
};

use crate::config::RolePermissionConfig;
use crate::membership::{MembershipChecker, parse_guild, parse_user};
use crate::store::LinkBook;

pub struct DiscordPermissionProvider {
    links: Arc<LinkBook>,
    membership: Arc<MembershipChecker>,
    mappings: RolePermissionConfig,
}

impl DiscordPermissionProvider {
    pub fn new(
        links: Arc<LinkBook>,
        membership: Arc<MembershipChecker>,
        mappings: RolePermissionConfig,
    ) -> Self {
        Self {
            links,
            membership,
            mappings,
        }
    }
}

impl PermissionProvider for DiscordPermissionProvider {
    fn create_checker<'a>(
        &'a self,
        subject: &'a PermissionSubject,
    ) -> infrarust_api::event::BoxFuture<'a, Arc<dyn PermissionChecker>> {
        Box::pin(async move {
            let mut grants = PermissionMap::new();
            let Some(uuid) = subject.profile().map(|profile| profile.uuid) else {
                return Arc::new(grants) as Arc<dyn PermissionChecker>;
            };
            let Some(discord_id) = self.links.linked_discord(uuid).await else {
                return Arc::new(grants) as Arc<dyn PermissionChecker>;
            };
            let Some(user) = parse_user(&discord_id) else {
                return Arc::new(grants) as Arc<dyn PermissionChecker>;
            };

            for (guild_raw, role_mappings) in &self.mappings.guild_roles {
                let Some(guild) = parse_guild(guild_raw) else {
                    tracing::warn!(guild = %guild_raw, "invalid Discord guild ID in role permission config");
                    continue;
                };
                let roles = match self.membership.roles(user, guild).await {
                    Ok(Some(roles)) => roles,
                    Ok(None) => continue,
                    Err(err) => {
                        tracing::warn!(uuid = %uuid, guild = %guild, error = %err, "Discord permission role lookup failed");
                        continue;
                    }
                };
                for (role_raw, nodes) in role_mappings {
                    let Some(role) = role_raw
                        .trim()
                        .parse::<NonZeroU64>()
                        .ok()
                        .map(NonZeroU64::get)
                    else {
                        tracing::warn!(role = %role_raw, "invalid Discord role ID in role permission config");
                        continue;
                    };
                    if roles.contains(&role) {
                        for node in nodes {
                            if valid_permission_node(node) {
                                grants.set(node, true);
                            } else {
                                tracing::warn!(node = %node, "invalid permission node in Discord role mapping");
                            }
                        }
                    }
                }
            }
            Arc::new(grants) as Arc<dyn PermissionChecker>
        })
    }
}

fn valid_permission_node(node: &str) -> bool {
    let node = node.trim();
    !node.eq_ignore_ascii_case("infrarust.admin")
        && !node.to_ascii_lowercase().starts_with("infrarust.admin.")
        && !node.is_empty()
        && node.len() <= 128
        && node.split('.').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
}
