//! Handshake framing, AES-128-GCM decryption, and the public entry point.
//!
//! Framing follows `AesCipher` plus the BASE64 topping upstream:
//! `^Floodgate^>` header, base64 IV (12 bytes), `!`, base64 ciphertext with
//! the 16-byte GCM tag appended. There is no AAD.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes128Gcm, KeyInit, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::bedrock_data;
use crate::error::FloodgateError;
use crate::identity::{BedrockIdentity, IdentityOptions};

/// Identifier that starts a Floodgate segment in the handshake hostname.
const IDENTIFIER: &str = "^Floodgate^";
/// Cipher version (`FloodgateCipher.VERSION` upstream). The header byte after
/// the identifier is `CIPHER_VERSION + 0x3E`, so version 0 is written as `>`.
const CIPHER_VERSION: u32 = 0;
const KEY_LEN: usize = 16;
const IV_LEN: usize = 12;
const TAG_LEN: usize = 16;
/// Upper bound on the encoded payload, after the header.
const MAX_ENCODED_LEN: usize = 8 * 1024;
/// Upper bound on the decrypted plaintext.
const MAX_PLAINTEXT_LEN: usize = 4 * 1024;

/// Decodes a Floodgate payload from a handshake hostname.
///
/// The hostname is split on NUL bytes. If no segment starts with the Floodgate
/// identifier, the host is an ordinary host and this returns `Ok(None)`. The
/// key is not checked in that case.
///
/// `key` must be the 16-byte raw AES-128 key shared with the Floodgate server.
///
/// # Errors
///
/// Returns a [`FloodgateError`] if the host has a marked segment that is
/// malformed, unsupported, or fails authentication, or if the options or key
/// are invalid.
pub fn decode_handshake(
    host: &str,
    key: &[u8],
    options: &IdentityOptions,
) -> Result<Option<BedrockIdentity>, FloodgateError> {
    let Some(payload) = find_payload(host)? else {
        return Ok(None);
    };

    options.validate()?;
    let key: [u8; KEY_LEN] = key
        .try_into()
        .map_err(|_| FloodgateError::InvalidKeyLength { actual: key.len() })?;

    let (iv, ciphertext) = split_payload(payload)?;
    let plaintext = decrypt(&key, &iv, &ciphertext)?;

    let text = std::str::from_utf8(&plaintext).map_err(|_| FloodgateError::InvalidUtf8)?;
    let data = bedrock_data::parse(text)?;

    Ok(Some(BedrockIdentity::new(
        data.username,
        data.xuid,
        data.ip,
        iv,
        options,
    )))
}

/// Returns the payload after the header, or `None` if the host has no
/// Floodgate segment. Fails if there is more than one segment, or if the
/// header's cipher version is not supported.
fn find_payload(host: &str) -> Result<Option<&str>, FloodgateError> {
    let mut found = None;
    for segment in host.split('\0') {
        let Some(rest) = segment.strip_prefix(IDENTIFIER) else {
            continue;
        };
        if found.is_some() {
            return Err(FloodgateError::MultipleSegments);
        }
        let mut chars = rest.chars();
        let version_char = chars.next().ok_or(FloodgateError::MalformedFraming)?;
        let actual = u32::from(version_char).wrapping_sub(0x3E);
        if actual != CIPHER_VERSION {
            return Err(FloodgateError::UnsupportedVersion {
                expected: CIPHER_VERSION,
                actual,
            });
        }
        found = Some(chars.as_str());
    }
    Ok(found)
}

/// Splits `{base64 iv}!{base64 ciphertext||tag}` and decodes both parts.
fn split_payload(payload: &str) -> Result<([u8; IV_LEN], Vec<u8>), FloodgateError> {
    if payload.len() > MAX_ENCODED_LEN {
        return Err(FloodgateError::PayloadTooLarge);
    }

    let (iv_b64, ct_b64) = payload
        .split_once('!')
        .ok_or(FloodgateError::MalformedFraming)?;
    if iv_b64.is_empty() || ct_b64.is_empty() {
        return Err(FloodgateError::MalformedFraming);
    }

    let iv_bytes = STANDARD
        .decode(iv_b64)
        .map_err(|_| FloodgateError::InvalidBase64)?;
    let iv: [u8; IV_LEN] =
        iv_bytes
            .as_slice()
            .try_into()
            .map_err(|_| FloodgateError::InvalidIvLength {
                actual: iv_bytes.len(),
            })?;

    let ciphertext = STANDARD
        .decode(ct_b64)
        .map_err(|_| FloodgateError::InvalidBase64)?;
    if ciphertext.len() < TAG_LEN {
        return Err(FloodgateError::CiphertextTooShort);
    }

    Ok((iv, ciphertext))
}

/// AES-128-GCM decryption with no AAD. Any authentication failure is reported
/// as `AuthenticationFailed`, so the error does not reveal why it failed.
fn decrypt(
    key: &[u8; KEY_LEN],
    iv: &[u8; IV_LEN],
    ciphertext: &[u8],
) -> Result<Vec<u8>, FloodgateError> {
    let cipher = Aes128Gcm::new(key.into());
    let nonce = Nonce::from(*iv);
    let plaintext = cipher
        .decrypt(&nonce, ciphertext)
        .map_err(|_| FloodgateError::AuthenticationFailed)?;
    if plaintext.len() > MAX_PLAINTEXT_LEN {
        return Err(FloodgateError::PlaintextTooLarge);
    }
    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::net::IpAddr;

    /// Header used by the test-only sealing helper.
    const MARKER: &str = "^Floodgate^>";
    const KEY: [u8; 16] = *b"0123456789abcdef";
    const IV: [u8; 12] = [7, 1, 9, 3, 5, 2, 8, 4, 6, 0, 11, 13];

    /// Test-only encryption. Mirrors the framing the decoder expects.
    fn seal_with(key: &[u8; 16], iv: &[u8; 12], plaintext: &[u8]) -> String {
        let cipher = Aes128Gcm::new(key.into());
        let nonce = Nonce::from(*iv);
        let ciphertext = cipher.encrypt(&nonce, plaintext).unwrap();
        format!(
            "{MARKER}{}!{}",
            STANDARD.encode(iv),
            STANDARD.encode(ciphertext)
        )
    }

    /// Builds a valid 12-field plaintext in upstream order, overriding indices.
    fn fields_with(overrides: &[(usize, &str)]) -> String {
        let mut fields = vec![
            "1.0.0".to_owned(),
            "Steve Player".to_owned(),
            "2535400000000001".to_owned(),
            "7".to_owned(),
            "en_US".to_owned(),
            "0".to_owned(),
            "1".to_owned(),
            "203.0.113.7".to_owned(),
            "null".to_owned(),
            "0".to_owned(),
            "0".to_owned(),
            "123456".to_owned(),
        ];
        for (index, value) in overrides {
            fields[*index] = (*value).to_owned();
        }
        fields.join("\0")
    }

    fn valid_host(plaintext: &str) -> String {
        seal_with(&KEY, &IV, plaintext.as_bytes())
    }

    fn decode(host: &str) -> Result<Option<BedrockIdentity>, FloodgateError> {
        decode_handshake(host, &KEY, &IdentityOptions::default())
    }

    #[test]
    fn valid_payload_decodes() {
        let host = format!("play.example.com\0{}", valid_host(&fields_with(&[])));
        let identity = decode(&host).unwrap().unwrap();

        assert_eq!(identity.bedrock_username(), "Steve Player");
        assert_eq!(identity.java_username(), ".Steve_Player");
        assert_eq!(identity.xuid(), 2535400000000001);
        assert_eq!(
            identity.uuid(),
            uuid::Uuid::from_u64_pair(0, 2535400000000001)
        );
        assert_eq!(identity.ip(), "203.0.113.7".parse::<IpAddr>().unwrap());
        assert_eq!(identity.replay_iv(), &IV);
    }

    #[test]
    fn independently_generated_reference_fixture_decodes() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/floodgate_handshake_v0.json"
        ))
        .unwrap();
        let key_hex = fixture["key"]["hex"].as_str().unwrap();
        let (pairs, remainder) = key_hex.as_bytes().as_chunks::<2>();
        assert!(remainder.is_empty());
        let key: Vec<u8> = pairs
            .iter()
            .map(|pair| {
                let hex = std::str::from_utf8(pair).unwrap();
                u8::from_str_radix(hex, 16).unwrap()
            })
            .collect();
        let host = fixture["raw_handshake_host"].as_str().unwrap();
        let options = IdentityOptions::default();

        let identity = decode_handshake(host, &key, &options).unwrap().unwrap();
        assert_eq!(identity.bedrock_username(), "Bedrock User");
        assert_eq!(identity.java_username(), ".Bedrock_User");
        assert_eq!(identity.xuid(), 2_535_400_000_000_001);
        assert_eq!(
            identity.uuid().to_string(),
            "00000000-0000-0000-0009-01eed05d1001"
        );
        assert_eq!(identity.ip(), "198.51.100.7".parse::<IpAddr>().unwrap());

        // Exercise beta.4's handshake parser with the independently sealed
        // marker: the current 255-character limit accepts the exact boundary.
        use infrarust_protocol::{ConnectionState, Packet, ProtocolVersion, SHandshake, VarInt};
        let marker = host.split_once('\0').unwrap().1;
        let protocol_round_trip = |server_address: String| {
            let packet = SHandshake {
                protocol_version: VarInt(767),
                server_address,
                server_port: 25565,
                next_state: ConnectionState::Login,
                transfer: false,
            };
            let mut bytes = Vec::new();
            packet.encode(&mut bytes, ProtocolVersion::V1_21).unwrap();
            SHandshake::decode(&mut bytes.as_slice(), ProtocolVersion::V1_21)
        };
        let cases = &fixture["length_calculator"]["cases"];
        let fits = format!(
            "{}\0{marker}",
            cases["near_limit_fits"]["hostname"].as_str().unwrap()
        );
        let exceeds = format!(
            "{}\0{marker}",
            cases["near_limit_exceeds"]["hostname"].as_str().unwrap()
        );
        assert_eq!(fits.chars().count(), 255);
        assert_eq!(exceeds.chars().count(), 256);
        assert!(protocol_round_trip(fits).is_ok());
        assert!(protocol_round_trip(exceeds).is_err());
    }

    #[test]
    fn from_proxy_flag_accepts_one() {
        let host = valid_host(&fields_with(&[(9, "1")]));
        assert!(decode(&host).unwrap().is_some());
    }

    #[test]
    fn plain_host_returns_none() {
        assert_eq!(decode("play.example.com").unwrap(), None);
        assert_eq!(decode("play.example.com\0FML2\0").unwrap(), None);
    }

    #[test]
    fn plain_host_ignores_key_length() {
        let result = decode_handshake("play.example.com", &[0u8; 3], &IdentityOptions::default());
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn marker_may_follow_other_segments() {
        let host = format!("a.example\0FML\0{}", valid_host(&fields_with(&[])));
        assert!(decode(&host).unwrap().is_some());
    }

    #[test]
    fn java_name_respects_options() {
        let host = valid_host(&fields_with(&[(1, "A Very Long Gamertag")]));

        let default = decode(&host).unwrap().unwrap();
        assert_eq!(default.java_username(), ".A_Very_Long_Gam");

        let opts = IdentityOptions {
            username_prefix: String::new(),
            replace_spaces: false,
        };
        let raw = decode_handshake(&host, &KEY, &opts).unwrap().unwrap();
        assert_eq!(raw.java_username(), "A Very Long Game");
        assert_eq!(raw.bedrock_username(), "A Very Long Gamertag");
    }

    #[test]
    fn wrong_key_fails_authentication() {
        let host = valid_host(&fields_with(&[]));
        let wrong = *b"fedcba9876543210";
        let err = decode_handshake(&host, &wrong, &IdentityOptions::default()).unwrap_err();
        assert_eq!(err, FloodgateError::AuthenticationFailed);
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let host = valid_host(&fields_with(&[]));
        let (prefix, payload) = host.split_once(MARKER).unwrap();
        let (iv, ct) = payload.split_once('!').unwrap();
        let mut bytes = STANDARD.decode(ct).unwrap();
        bytes[0] ^= 0x01;
        let tampered = format!("{prefix}{MARKER}{iv}!{}", STANDARD.encode(bytes));
        assert_eq!(
            decode(&tampered).unwrap_err(),
            FloodgateError::AuthenticationFailed
        );
    }

    #[test]
    fn tampered_tag_fails() {
        let host = valid_host(&fields_with(&[]));
        let (iv, ct) = host.strip_prefix(MARKER).unwrap().split_once('!').unwrap();
        let mut bytes = STANDARD.decode(ct).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x80;
        let tampered = format!("{MARKER}{iv}!{}", STANDARD.encode(bytes));
        assert_eq!(
            decode(&tampered).unwrap_err(),
            FloodgateError::AuthenticationFailed
        );
    }

    #[test]
    fn swapped_iv_fails() {
        let host = valid_host(&fields_with(&[]));
        let (_, ct) = host.strip_prefix(MARKER).unwrap().split_once('!').unwrap();
        let other_iv = [0u8; 12];
        let tampered = format!("{MARKER}{}!{ct}", STANDARD.encode(other_iv));
        assert_eq!(
            decode(&tampered).unwrap_err(),
            FloodgateError::AuthenticationFailed
        );
    }

    #[test]
    fn unsupported_header_version_is_rejected() {
        let host = valid_host(&fields_with(&[]));
        let tampered = host.replacen("^Floodgate^>", "^Floodgate^?", 1);
        assert_eq!(
            decode(&tampered).unwrap_err(),
            FloodgateError::UnsupportedVersion {
                expected: 0,
                actual: 1
            }
        );
    }

    #[test]
    fn missing_header_version_is_malformed() {
        assert_eq!(
            decode("^Floodgate^").unwrap_err(),
            FloodgateError::MalformedFraming
        );
    }

    #[test]
    fn missing_separator_is_malformed() {
        let host = format!("{MARKER}AAAAAAAAAAAAAAAA");
        assert_eq!(decode(&host).unwrap_err(), FloodgateError::MalformedFraming);
    }

    #[test]
    fn empty_parts_are_malformed() {
        assert_eq!(
            decode(&format!("{MARKER}!AAAA")).unwrap_err(),
            FloodgateError::MalformedFraming
        );
        assert_eq!(
            decode(&format!("{MARKER}AAAA!")).unwrap_err(),
            FloodgateError::MalformedFraming
        );
    }

    #[test]
    fn invalid_base64_is_rejected() {
        let host = format!("{MARKER}{}!@@@@@@@@", STANDARD.encode(IV));
        assert_eq!(decode(&host).unwrap_err(), FloodgateError::InvalidBase64);
    }

    #[test]
    fn wrong_iv_length_is_rejected() {
        let host = format!(
            "{MARKER}{}!{}",
            STANDARD.encode([0u8; 8]),
            STANDARD.encode([0u8; 32])
        );
        assert_eq!(
            decode(&host).unwrap_err(),
            FloodgateError::InvalidIvLength { actual: 8 }
        );
    }

    #[test]
    fn short_ciphertext_is_rejected() {
        let host = format!(
            "{MARKER}{}!{}",
            STANDARD.encode(IV),
            STANDARD.encode([0u8; 8])
        );
        assert_eq!(
            decode(&host).unwrap_err(),
            FloodgateError::CiphertextTooShort
        );
    }

    #[test]
    fn oversized_payload_is_rejected() {
        let host = format!("{MARKER}{}", "A".repeat(MAX_ENCODED_LEN + 1));
        assert_eq!(decode(&host).unwrap_err(), FloodgateError::PayloadTooLarge);
    }

    #[test]
    fn invalid_key_length_is_rejected_for_marked_host() {
        let host = valid_host(&fields_with(&[]));
        let err = decode_handshake(&host, &[0u8; 32], &IdentityOptions::default()).unwrap_err();
        assert_eq!(err, FloodgateError::InvalidKeyLength { actual: 32 });
    }

    #[test]
    fn invalid_options_are_rejected_for_marked_host() {
        let host = valid_host(&fields_with(&[]));
        let opts = IdentityOptions {
            username_prefix: "0123456789abcdef".to_owned(),
            replace_spaces: true,
        };
        let err = decode_handshake(&host, &KEY, &opts).unwrap_err();
        assert!(matches!(err, FloodgateError::InvalidOptions { .. }));
    }

    #[test]
    fn multiple_segments_are_rejected() {
        let seg = valid_host(&fields_with(&[]));
        let host = format!("{seg}\0{seg}");
        assert_eq!(decode(&host).unwrap_err(), FloodgateError::MultipleSegments);
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        let mut plain = fields_with(&[]).into_bytes();
        plain.insert(0, 0xFF);
        let host = seal_with(&KEY, &IV, &plain);
        assert_eq!(decode(&host).unwrap_err(), FloodgateError::InvalidUtf8);
    }

    #[test]
    fn missing_field_is_rejected() {
        let plain = fields_with(&[]);
        let trimmed: Vec<&str> = plain.split('\0').take(11).collect();
        let host = valid_host(&trimmed.join("\0"));
        assert_eq!(
            decode(&host).unwrap_err(),
            FloodgateError::FieldCount { actual: 11 }
        );
    }

    #[test]
    fn excess_field_is_rejected() {
        let host = valid_host(&format!("{}\0extra", fields_with(&[])));
        assert_eq!(
            decode(&host).unwrap_err(),
            FloodgateError::FieldCount { actual: 13 }
        );
    }

    #[test]
    fn empty_version_field_is_rejected() {
        let host = valid_host(&fields_with(&[(0, "")]));
        assert_eq!(
            decode(&host).unwrap_err(),
            FloodgateError::InvalidField { field: "version" }
        );
    }

    #[test]
    fn invalid_xuid_is_rejected() {
        for bad in ["", "0", "-5", "12a", "+5", "18446744073709551616"] {
            let host = valid_host(&fields_with(&[(2, bad)]));
            assert_eq!(
                decode(&host).unwrap_err(),
                FloodgateError::InvalidField { field: "xuid" },
                "xuid {bad:?}"
            );
        }
    }

    #[test]
    fn invalid_ip_is_rejected() {
        let host = valid_host(&fields_with(&[(7, "not-an-ip")]));
        assert_eq!(
            decode(&host).unwrap_err(),
            FloodgateError::InvalidField { field: "ip" }
        );
    }

    #[test]
    fn invalid_username_is_rejected() {
        let long = "x".repeat(33);
        for bad in ["", long.as_str(), "bad\u{7}name"] {
            let host = valid_host(&fields_with(&[(1, bad)]));
            assert_eq!(
                decode(&host).unwrap_err(),
                FloodgateError::InvalidField { field: "username" }
            );
        }
    }

    #[test]
    fn linked_player_is_rejected() {
        let host = valid_host(&fields_with(&[(8, "{\"java\":\"Steve\"}")]));
        assert_eq!(
            decode(&host).unwrap_err(),
            FloodgateError::LinkedPlayerUnsupported
        );
    }

    #[test]
    fn empty_link_field_is_rejected() {
        let host = valid_host(&fields_with(&[(8, "")]));
        assert_eq!(
            decode(&host).unwrap_err(),
            FloodgateError::LinkedPlayerUnsupported
        );
    }

    #[test]
    fn invalid_from_proxy_is_rejected() {
        for bad in ["true", "2", ""] {
            let host = valid_host(&fields_with(&[(9, bad)]));
            assert_eq!(
                decode(&host).unwrap_err(),
                FloodgateError::InvalidField {
                    field: "from proxy"
                },
                "from proxy {bad:?}"
            );
        }
    }

    #[test]
    fn non_numeric_integer_field_is_rejected() {
        let host = valid_host(&fields_with(&[(3, "seven")]));
        assert_eq!(
            decode(&host).unwrap_err(),
            FloodgateError::InvalidField { field: "device os" }
        );
    }

    #[test]
    fn empty_verify_code_is_rejected() {
        let host = valid_host(&fields_with(&[(11, "")]));
        assert_eq!(
            decode(&host).unwrap_err(),
            FloodgateError::InvalidField {
                field: "verify code"
            }
        );
    }

    #[test]
    fn errors_do_not_leak_secrets() {
        let host = valid_host(&fields_with(&[(2, "nope")]));
        let wrong = *b"fedcba9876543210";
        let errors = [
            decode(&host).unwrap_err(),
            decode_handshake(&host, &wrong, &IdentityOptions::default()).unwrap_err(),
            decode(&format!("{MARKER}!")).unwrap_err(),
        ];
        for err in errors {
            let text = format!("{err} {err:?}");
            assert!(!text.contains("0123456789abcdef"));
            assert!(!text.contains("Steve"));
            assert!(!text.contains("2535400000000001"));
        }
    }

    #[test]
    fn identity_debug_is_redacted() {
        let host = valid_host(&fields_with(&[]));
        let identity = decode(&host).unwrap().unwrap();
        let text = format!("{identity:?}");
        assert!(text.contains("<redacted>"));
        assert!(!text.contains("Steve"));
        assert!(!text.contains(".Steve_Player"));
        assert!(!text.contains("2535400000000001"));
        assert!(!text.contains("203.0.113.7"));
    }
}
