//! Error type for Floodgate handshake decoding.

use thiserror::Error;

/// Errors returned when a marked Floodgate handshake cannot be decoded.
///
/// No variant carries key material, plaintext, or identity values, so the
/// `Display` and `Debug` output is safe to log.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum FloodgateError {
    /// The key is not exactly 16 bytes.
    #[error("invalid Floodgate key length: expected 16 bytes, got {actual}")]
    InvalidKeyLength {
        /// Length of the key that was supplied.
        actual: usize,
    },

    /// The handshake host contains more than one Floodgate segment.
    #[error("handshake host contains multiple Floodgate segments")]
    MultipleSegments,

    /// The Floodgate segment is missing its `!` separator or has an empty part.
    #[error("malformed Floodgate payload framing")]
    MalformedFraming,

    /// The encoded payload exceeds the size limit.
    #[error("Floodgate payload exceeds the size limit")]
    PayloadTooLarge,

    /// A base64 section failed to decode.
    #[error("invalid base64 in Floodgate payload")]
    InvalidBase64,

    /// The decoded IV is not 12 bytes.
    #[error("invalid Floodgate IV length: expected 12 bytes, got {actual}")]
    InvalidIvLength {
        /// Length of the decoded IV.
        actual: usize,
    },

    /// The ciphertext is too short to contain the GCM tag.
    #[error("Floodgate ciphertext is shorter than the authentication tag")]
    CiphertextTooShort,

    /// AES-GCM authentication failed (wrong key, or tampered data).
    #[error("Floodgate payload failed authentication")]
    AuthenticationFailed,

    /// The decrypted plaintext exceeds the size limit.
    #[error("Floodgate plaintext exceeds the size limit")]
    PlaintextTooLarge,

    /// The decrypted plaintext is not valid UTF-8.
    #[error("Floodgate plaintext is not valid UTF-8")]
    InvalidUtf8,

    /// The header cipher version does not match the version this crate supports.
    #[error("unsupported Floodgate cipher version: expected {expected}, got {actual}")]
    UnsupportedVersion {
        /// Version this crate implements.
        expected: u32,
        /// Version found in the payload.
        actual: u32,
    },

    /// The plaintext does not contain exactly 12 fields.
    #[error("Floodgate data has {actual} fields, expected 12")]
    FieldCount {
        /// Number of fields found.
        actual: usize,
    },

    /// A field failed validation. The name identifies the field, not its value.
    #[error("invalid Floodgate field: {field}")]
    InvalidField {
        /// Name of the field that failed validation.
        field: &'static str,
    },

    /// The payload carries a linked Java player, which is not supported.
    #[error("linked Floodgate players are not supported")]
    LinkedPlayerUnsupported,

    /// The identity options are invalid.
    #[error("invalid identity options: {reason}")]
    InvalidOptions {
        /// Human-readable reason. Contains no user data.
        reason: &'static str,
    },
}
