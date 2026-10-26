//! Public identity types and Java username derivation.

use std::fmt;
use std::net::IpAddr;

use uuid::Uuid;

use crate::error::FloodgateError;

/// Maximum length of a Java username.
pub(crate) const JAVA_USERNAME_MAX: usize = 16;

/// Options that control how the Java username is derived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityOptions {
    /// Prefix prepended to the Bedrock username, for example `"."`.
    pub username_prefix: String,
    /// Replace spaces in the Bedrock username with `_`.
    pub replace_spaces: bool,
}

impl Default for IdentityOptions {
    fn default() -> Self {
        Self {
            username_prefix: ".".to_owned(),
            replace_spaces: true,
        }
    }
}

impl IdentityOptions {
    /// Checks the options. Called before any Floodgate payload is decoded.
    pub(crate) fn validate(&self) -> Result<(), FloodgateError> {
        if self.username_prefix.chars().count() >= JAVA_USERNAME_MAX {
            return Err(FloodgateError::InvalidOptions {
                reason: "username prefix must be shorter than 16 characters",
            });
        }
        Ok(())
    }
}

/// A Bedrock player identity decoded from a Floodgate handshake.
///
/// `Debug` redacts the username, Java username, XUID, UUID, IP address, and
/// IV, so the value can be logged without leaking identity data.
#[derive(Clone, PartialEq, Eq)]
pub struct BedrockIdentity {
    bedrock_username: String,
    java_username: String,
    xuid: u64,
    ip: IpAddr,
    replay_iv: [u8; 12],
}

impl BedrockIdentity {
    pub(crate) fn new(
        bedrock_username: String,
        xuid: u64,
        ip: IpAddr,
        replay_iv: [u8; 12],
        options: &IdentityOptions,
    ) -> Self {
        let java_username = derive_java_username(&bedrock_username, options);
        Self {
            bedrock_username,
            java_username,
            xuid,
            ip,
            replay_iv,
        }
    }

    /// The Bedrock username as sent by the client.
    pub fn bedrock_username(&self) -> &str {
        &self.bedrock_username
    }

    /// The canonical Java username, with the prefix applied and the length
    /// capped at 16 characters.
    pub fn java_username(&self) -> &str {
        &self.java_username
    }

    /// The Xbox user ID.
    pub fn xuid(&self) -> u64 {
        self.xuid
    }

    /// The UUID for this identity, `UUID(0, xuid)`.
    pub fn uuid(&self) -> Uuid {
        Uuid::from_u64_pair(0, self.xuid)
    }

    /// The Bedrock client's IP address, as reported in the payload.
    pub fn ip(&self) -> IpAddr {
        self.ip
    }

    /// The 12-byte IV from the payload. Use it to detect replayed handshakes.
    pub fn replay_iv(&self) -> &[u8; 12] {
        &self.replay_iv
    }
}

impl fmt::Debug for BedrockIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BedrockIdentity")
            .field("bedrock_username", &"<redacted>")
            .field("java_username", &"<redacted>")
            .field("xuid", &"<redacted>")
            .field("ip", &"<redacted>")
            .field("replay_iv", &"<redacted>")
            .finish()
    }
}

/// Builds the Java username: prefix, then the Bedrock name (with spaces
/// optionally replaced), truncated to 16 characters.
fn derive_java_username(bedrock_username: &str, options: &IdentityOptions) -> String {
    let mut name = String::with_capacity(options.username_prefix.len() + bedrock_username.len());
    name.push_str(&options.username_prefix);
    if options.replace_spaces {
        name.extend(
            bedrock_username
                .chars()
                .map(|c| if c == ' ' { '_' } else { c }),
        );
    } else {
        name.push_str(bedrock_username);
    }
    name.chars().take(JAVA_USERNAME_MAX).collect()
}
