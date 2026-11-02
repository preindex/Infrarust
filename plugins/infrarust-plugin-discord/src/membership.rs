//! Guild membership lookups through the targeted member endpoint.
//!
//! This uses `GET /guilds/{guild}/members/{user}`, so no privileged member-list
//! intent is needed. Only a confirmed answer is cached: `Member`, or
//! `NotMember` from a 404. Every other failure is `LookupError` and must be
//! treated as unavailable, never as an allow or a deny.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use twilight_http::Client;
use twilight_http::api_error::ApiError;
use twilight_http::error::ErrorType;
use twilight_model::id::Id;
use twilight_model::id::marker::{GuildMarker, UserMarker};

use crate::config::MembershipConfig;

/// Discord error code for "Unknown Member".
const UNKNOWN_MEMBER: u64 = 10007;
/// Discord error code for "Unknown Guild": the guild is wrong or the bot is not in it.
const UNKNOWN_GUILD: u64 = 10004;
const CACHE_SWEEP_THRESHOLD: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Membership {
    Member,
    NotMember,
}

#[derive(Debug, thiserror::Error)]
pub enum LookupError {
    #[error("Discord rate limited the membership lookup")]
    RateLimited,
    #[error("Discord returned status {0}")]
    Status(u16),
    #[error("the guild is unknown or the bot is not in it")]
    UnknownGuild,
    #[error("the Discord request failed: {0}")]
    Transport(String),
}

pub struct MembershipChecker {
    http: Arc<Client>,
    cache: Mutex<HashMap<(u64, u64), (CachedMembership, Instant)>>,
    positive_ttl: Duration,
    negative_ttl: Duration,
}

#[derive(Clone, Debug)]
enum CachedMembership {
    Member(HashSet<u64>),
    NotMember,
}

impl MembershipChecker {
    pub fn new(http: Arc<Client>, config: &MembershipConfig) -> Self {
        Self {
            http,
            cache: Mutex::new(HashMap::new()),
            positive_ttl: config.positive_ttl(),
            negative_ttl: config.negative_ttl(),
        }
    }

    pub async fn check(
        &self,
        user: Id<UserMarker>,
        guild: Id<GuildMarker>,
    ) -> Result<Membership, LookupError> {
        Ok(match self.roles(user, guild).await? {
            Some(_) => Membership::Member,
            None => Membership::NotMember,
        })
    }

    /// Returns the member's roles, or `None` only for a confirmed absence.
    /// All transport, permission and rate-limit failures remain errors.
    pub async fn roles(
        &self,
        user: Id<UserMarker>,
        guild: Id<GuildMarker>,
    ) -> Result<Option<HashSet<u64>>, LookupError> {
        let key = (user.get(), guild.get());
        if let Some(cached) = self.cached(key) {
            return Ok(match cached {
                CachedMembership::Member(roles) => Some(roles),
                CachedMembership::NotMember => None,
            });
        }
        let roles = match self.http.guild_member(guild, user).await {
            Ok(response) => {
                let member = response
                    .model()
                    .await
                    .map_err(|err| LookupError::Transport(err.to_string()))?;
                Some(member.roles.into_iter().map(|role| role.get()).collect())
            }
            Err(err) => match classify(&err) {
                Ok(Membership::NotMember) => None,
                Ok(Membership::Member) => {
                    return Err(LookupError::Transport(
                        "Discord returned an unexpected member status".into(),
                    ));
                }
                Err(err) => return Err(err),
            },
        };
        self.store(key, roles.clone());
        Ok(roles)
    }

    fn cached(&self, key: (u64, u64)) -> Option<CachedMembership> {
        let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        cache
            .get(&key)
            .filter(|(_, until)| Instant::now() < *until)
            .map(|(membership, _)| membership.clone())
    }

    fn store(&self, key: (u64, u64), roles: Option<HashSet<u64>>) {
        let (membership, ttl) = match roles {
            Some(roles) => (CachedMembership::Member(roles), self.positive_ttl),
            None => (CachedMembership::NotMember, self.negative_ttl),
        };
        let now = Instant::now();
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if cache.len() > CACHE_SWEEP_THRESHOLD {
            cache.retain(|_, (_, until)| now < *until);
        }
        cache.insert(key, (membership, now + ttl));
    }
}

/// Maps a failed request to an answer. `Ok(NotMember)` only for a confirmed
/// absence; everything else is `Err`.
fn classify(err: &twilight_http::Error) -> Result<Membership, LookupError> {
    match err.kind() {
        ErrorType::Response { error, status, .. } => {
            let code = match error {
                ApiError::General(general) => Some(general.code),
                _ => None,
            };
            if code == Some(UNKNOWN_GUILD) {
                return Err(LookupError::UnknownGuild);
            }
            if code == Some(UNKNOWN_MEMBER) {
                return Ok(Membership::NotMember);
            }
            if status.get() == 429 || matches!(error, ApiError::Ratelimited(_)) {
                return Err(LookupError::RateLimited);
            }
            Err(LookupError::Status(status.get()))
        }
        _ => Err(LookupError::Transport(format!("{:?}", err.kind()))),
    }
}

/// Parses a configured guild ID. Zero and non-numeric values are rejected.
pub fn parse_guild(raw: &str) -> Option<Id<GuildMarker>> {
    raw.trim()
        .parse::<NonZeroU64>()
        .ok()
        .map(Id::<GuildMarker>::from)
}

/// Parses a Discord user ID. Zero and non-numeric values are rejected.
pub fn parse_user(raw: &str) -> Option<Id<UserMarker>> {
    raw.trim()
        .parse::<NonZeroU64>()
        .ok()
        .map(Id::<UserMarker>::from)
}
