//! Pure Rust codec for the Floodgate handshake marker.
//!
//! Floodgate (and Geyser's shared `common` module) lets a Bedrock client
//! identify itself through the handshake hostname. The hostname is split on
//! NUL bytes, and one segment carries an encrypted payload of the form:
//!
//! ```text
//! ^Floodgate^>{base64(iv, 12 bytes)}!{base64(ciphertext || tag)}
//! ```
//!
//! The payload is AES-128-GCM encrypted with a 16-byte raw key and no
//! additional authenticated data. The plaintext is 12 NUL-separated
//! `BedrockData` fields.
//!
//! The entry point is [`decode_handshake`]. It returns `Ok(None)` for an
//! ordinary host that carries no Floodgate segment, `Ok(Some(_))` for a
//! valid Bedrock identity, and `Err(_)` for a marked host that is malformed,
//! unsupported, or fails authentication.
//!
//! Linked-player data is out of scope. Only an absent or `null` link is
//! accepted; a linked identity is rejected with
//! [`FloodgateError::LinkedPlayerUnsupported`].
//!
//! This crate contains no `unsafe` code. Error messages never include the
//! key, the plaintext, or the identity values.

mod bedrock_data;
mod codec;
mod error;
mod identity;

pub use codec::decode_handshake;
pub use error::FloodgateError;
pub use identity::{BedrockIdentity, IdentityOptions};
