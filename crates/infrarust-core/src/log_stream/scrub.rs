//! Best-effort redaction for log messages before they leave the proxy.
//!
//! Two things are removed: dotted-quad IPv4 addresses, and values assigned to
//! keys that end in a secret word (`password`, `token`, `secret`, and so on).
//! IPv6 addresses and secrets with no recognizable key are not caught.

const IP_REDACTED: &str = "[ip]";
const SECRET_REDACTED: &str = "[redacted]";

/// Key suffixes whose values are redacted. Matched case-insensitively.
const SECRET_SUFFIXES: &[&str] = &[
    "password",
    "passwd",
    "token",
    "secret",
    "api_key",
    "apikey",
    "api-key",
    "authorization",
];

pub fn scrub(input: &str) -> String {
    redact_secrets(&redact_ipv4(input))
}

/// Replaces each maximal run of digits and dots that parses as IPv4.
/// Surrounding dots are kept, so a sentence-final `.` is not swallowed.
fn redact_ipv4(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut run = String::new();
    for ch in input.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            run.push(ch);
        } else {
            push_run(&mut out, &run);
            run.clear();
            out.push(ch);
        }
    }
    push_run(&mut out, &run);
    out
}

fn push_run(out: &mut String, run: &str) {
    let lead = run.len() - run.trim_start_matches('.').len();
    let trail = run.len() - run.trim_end_matches('.').len();
    if lead + trail >= run.len() {
        out.push_str(run);
        return;
    }
    let core = &run[lead..run.len() - trail];
    if is_ipv4(core) {
        out.push_str(&run[..lead]);
        out.push_str(IP_REDACTED);
        out.push_str(&run[run.len() - trail..]);
    } else {
        out.push_str(run);
    }
}

fn is_ipv4(candidate: &str) -> bool {
    let parts: Vec<&str> = candidate.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|part| {
            (1..=3).contains(&part.len())
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && part.parse::<u8>().is_ok()
        })
}

/// Redacts the value after `key=` or `key:` when the key is a secret.
/// Handles `token=abc&x=1`, `password: hunter2`, and `"token": "abc"`.
fn redact_secrets(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut copied = 0;
    let mut index = 0;
    while index < bytes.len() {
        if !matches!(bytes[index], b'=' | b':') {
            index += 1;
            continue;
        }
        // JSON keys close their quote before the colon: `"token": "abc"`.
        let key_end = if index > 0 && matches!(bytes[index - 1], b'"' | b'\'') {
            index - 1
        } else {
            index
        };
        let key_start = bytes[..key_end]
            .iter()
            .rposition(|byte| !is_key_byte(*byte))
            .map_or(0, |position| position + 1);
        let key = &input[key_start..key_end];

        let mut value_start = index + 1;
        while bytes.get(value_start) == Some(&b' ') {
            value_start += 1;
        }
        let quote = bytes
            .get(value_start)
            .copied()
            .filter(|byte| matches!(byte, b'"' | b'\''));
        if quote.is_some() {
            value_start += 1;
        }
        let mut value_end = value_start;
        while value_end < bytes.len() && !is_value_end(bytes[value_end], quote) {
            value_end += 1;
        }

        if key_start >= copied && !key.is_empty() && value_end > value_start && is_secret_key(key) {
            out.push_str(&input[copied..value_start]);
            out.push_str(SECRET_REDACTED);
            copied = value_end;
            index = value_end;
        } else {
            index += 1;
        }
    }
    out.push_str(&input[copied..]);
    out
}

const fn is_key_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

fn is_value_end(byte: u8, quote: Option<u8>) -> bool {
    match quote {
        Some(closing) => byte == closing,
        None => {
            byte.is_ascii_whitespace()
                || matches!(byte, b'&' | b',' | b';' | b'"' | b'\'' | b')' | b']' | b'}')
        }
    }
}

fn is_secret_key(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    SECRET_SUFFIXES
        .iter()
        .any(|suffix| lowered.ends_with(suffix))
}
