//! Parsing and validation of the decrypted `BedrockData` plaintext.
//!
//! This follows `BedrockData.fromString` / `toString` in GeyserMC's
//! `common/src/main/java/org/geysermc/floodgate/util/BedrockData.java`. The
//! plaintext holds exactly 12 NUL-separated fields, in this order:
//!
//! | index | field          | encoding                                  |
//! |-------|----------------|-------------------------------------------|
//! | 0     | version        | string (non-empty)                        |
//! | 1     | username       | string (Bedrock username)                 |
//! | 2     | xuid           | unsigned 64-bit decimal                   |
//! | 3     | device OS      | integer                                   |
//! | 4     | language code  | string                                    |
//! | 5     | UI profile     | integer                                   |
//! | 6     | input mode     | integer                                   |
//! | 7     | ip             | IPv4 or IPv6 address                      |
//! | 8     | linked player  | `"null"` only (linked players unsupported)|
//! | 9     | from proxy     | `"1"` or `"0"`                            |
//! | 10    | subscribe ID   | integer                                   |
//! | 11    | verify code    | string (non-empty)                        |

use std::net::IpAddr;

use crate::error::FloodgateError;

/// Number of fields in a valid `BedrockData` plaintext.
pub(crate) const FIELD_COUNT: usize = 12;

const MAX_USERNAME_CHARS: usize = 32;
const MAX_VERSION_CHARS: usize = 32;
const MAX_LANGUAGE_CHARS: usize = 16;
const MAX_VERIFY_CODE_CHARS: usize = 32;

/// Validated fields that the identity needs.
#[derive(Debug)]
pub(crate) struct BedrockData {
    pub(crate) username: String,
    pub(crate) xuid: u64,
    pub(crate) ip: IpAddr,
}

/// Parses and validates the decrypted plaintext.
pub(crate) fn parse(text: &str) -> Result<BedrockData, FloodgateError> {
    let fields: Vec<&str> = text.split('\0').collect();
    if fields.len() != FIELD_COUNT {
        return Err(FloodgateError::FieldCount {
            actual: fields.len(),
        });
    }

    check_text(fields[0], "version", MAX_VERSION_CHARS)?;
    let username = check_text(fields[1], "username", MAX_USERNAME_CHARS)?;
    let xuid = parse_xuid(fields[2])?;
    let _device_os: i32 = parse_number(fields[3], "device os")?;
    check_text(fields[4], "language code", MAX_LANGUAGE_CHARS)?;
    let _ui_profile: i32 = parse_number(fields[5], "ui profile")?;
    let _input_mode: i32 = parse_number(fields[6], "input mode")?;
    let ip = fields[7]
        .parse::<IpAddr>()
        .map_err(|_| FloodgateError::InvalidField { field: "ip" })?;

    match fields[8] {
        "null" => {}
        _ => return Err(FloodgateError::LinkedPlayerUnsupported),
    }

    match fields[9] {
        "0" | "1" => {}
        _ => {
            return Err(FloodgateError::InvalidField {
                field: "from proxy",
            });
        }
    }

    let _subscribe_id: i32 = parse_number(fields[10], "subscribe id")?;
    check_text(fields[11], "verify code", MAX_VERIFY_CODE_CHARS)?;

    Ok(BedrockData {
        username: username.to_owned(),
        xuid,
        ip,
    })
}

/// Parses a plain decimal integer. Rejects `+`, whitespace, and empty input.
fn parse_number<T: std::str::FromStr>(
    value: &str,
    field: &'static str,
) -> Result<T, FloodgateError> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
        return Err(FloodgateError::InvalidField { field });
    }
    value
        .parse::<T>()
        .map_err(|_| FloodgateError::InvalidField { field })
}

fn parse_xuid(value: &str) -> Result<u64, FloodgateError> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(FloodgateError::InvalidField { field: "xuid" });
    }
    let xuid: u64 = value
        .parse()
        .map_err(|_| FloodgateError::InvalidField { field: "xuid" })?;
    if xuid == 0 {
        return Err(FloodgateError::InvalidField { field: "xuid" });
    }
    Ok(xuid)
}

/// Checks that a string is non-empty, within `max_chars`, and has no control
/// characters.
fn check_text<'a>(
    value: &'a str,
    field: &'static str,
    max_chars: usize,
) -> Result<&'a str, FloodgateError> {
    let count = value.chars().count();
    if count == 0 || count > max_chars || value.chars().any(char::is_control) {
        return Err(FloodgateError::InvalidField { field });
    }
    Ok(value)
}
