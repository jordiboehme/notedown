//! Masking of secrets and size-capping of bodies.

use std::collections::BTreeMap;

use serde_json::Value;

/// Bodies larger than this (serialized) are cut.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

const SENSITIVE: [&str; 8] = [
    "password",
    "passwordmd5",
    "randomcode",
    "token",
    "sign",
    "x-access-token",
    "code",
    "validcode",
];

/// Headers worth keeping in the protocol log.
pub const SELECTED_HEADERS: [&str; 8] = [
    "content-type",
    "content-length",
    "user-agent",
    "host",
    "x-access-token",
    "equipmentno",
    "version",
    "accept",
];

/// Whether a JSON key, query key or header name carries a secret (case-insensitive).
pub fn is_sensitive_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    SENSITIVE.contains(&k.as_str())
}

const PREFIX: &str = "<masked:";

fn placeholder(chars: usize) -> String {
    format!("{PREFIX}{chars} chars>")
}

fn mask_value(v: &Value) -> Value {
    match v {
        // Already masked: keep, so masking twice is stable.
        Value::String(s) if s.starts_with(PREFIX) && s.ends_with(" chars>") => v.clone(),
        Value::String(s) => Value::String(placeholder(s.chars().count())),
        Value::Null => Value::Null,
        other => Value::String(placeholder(other.to_string().chars().count())),
    }
}

/// Mask the values of sensitive keys at any depth.
pub fn mask_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    if is_sensitive_key(k) {
                        (k.clone(), mask_value(v))
                    } else {
                        (k.clone(), mask_json(v))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(mask_json).collect()),
        other => other.clone(),
    }
}

/// Mask sensitive values in a raw query string (`a=b&c=d`).
pub fn mask_query(query: &str) -> String {
    query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) if is_sensitive_key(k) => {
                if v.starts_with(PREFIX) {
                    pair.to_string()
                } else {
                    format!("{k}={}", placeholder(v.chars().count()))
                }
            }
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Mask sensitive header values (names compared case-insensitively).
pub fn mask_headers(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(k, v)| {
            if is_sensitive_key(k) && !v.starts_with(PREFIX) {
                (k.clone(), placeholder(v.chars().count()))
            } else {
                (k.clone(), v.clone())
            }
        })
        .collect()
}

/// Keep only the headers in [`SELECTED_HEADERS`], names lowercased.
pub fn select_headers<'a>(
    all: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> BTreeMap<String, String> {
    all.into_iter()
        .filter_map(|(k, v)| {
            let k = k.to_ascii_lowercase();
            SELECTED_HEADERS
                .contains(&k.as_str())
                .then(|| (k, v.to_string()))
        })
        .collect()
}

/// Cut a body larger than [`MAX_BODY_BYTES`]: it becomes a string holding a prefix and a note.
/// Mask before cutting so the prefix cannot leak a secret.
pub fn cut_body(value: &Value) -> Value {
    let text = value.to_string();
    if text.len() <= MAX_BODY_BYTES {
        return value.clone();
    }
    let mut end = MAX_BODY_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Value::String(format!(
        "{}... <cut: {} of {} bytes kept>",
        &text[..end],
        end,
        text.len()
    ))
}
