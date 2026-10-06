//! Masking of secrets and size-capping of bodies. One placeholder helper and one key rule are
//! shared by JSON bodies, query strings, form bodies, embedded JSON strings and headers.

use std::collections::BTreeMap;

use serde_json::Value;

/// Bodies larger than this (serialized) are cut.
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// Path, query and every header value are cut at this length.
pub const MAX_FIELD_BYTES: usize = 4 * 1024;

/// A key is sensitive when its lowercase name contains one of these ...
const SENSITIVE_PARTS: [&str; 8] = [
    "password",
    "token",
    "secret",
    "authorization",
    "cookie",
    "sign",
    "randomcode",
    "validcode",
];

const PREFIX: &str = "<masked:";
const SUFFIX: &str = " chars>";

/// Whether a JSON key, query key or form key carries a secret (case-insensitive): the name
/// contains password, token, secret, authorization, cookie, sign, randomcode or validcode, or is
/// exactly `code`.
pub fn is_sensitive_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k == "code" || SENSITIVE_PARTS.iter().any(|p| k.contains(p))
}

/// Header names: the key rule plus anything containing `auth`.
pub fn is_sensitive_header(name: &str) -> bool {
    is_sensitive_key(name) || name.to_ascii_lowercase().contains("auth")
}

/// The one placeholder: `<masked:N chars>`.
pub fn placeholder(chars: usize) -> String {
    format!("{PREFIX}{chars}{SUFFIX}")
}

/// Whether a string already is a placeholder (masking twice is stable).
pub fn is_masked(s: &str) -> bool {
    s.starts_with(PREFIX) && s.ends_with(SUFFIX)
}

fn mask_str(s: &str) -> String {
    if is_masked(s) {
        s.to_string()
    } else {
        placeholder(s.chars().count())
    }
}

fn mask_value(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(mask_str(s)),
        Value::Null => Value::Null,
        other => Value::String(placeholder(other.to_string().chars().count())),
    }
}

/// Mask the values of sensitive keys at any depth. A string value that holds JSON (after an
/// optional numeric Engine.IO prefix such as `42`) or urlencoded pairs is masked inside; when that
/// changes anything, the string is replaced by the masked text (still a string).
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
        Value::String(s) => Value::String(mask_text(s)),
        other => other.clone(),
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn looks_like_form(s: &str) -> bool {
    !s.is_empty()
        && s.split('&').all(|p| match p.split_once('=') {
            Some((k, _)) => {
                !k.is_empty()
                    && k.chars().all(|c| {
                        c.is_ascii_alphanumeric()
                            || matches!(c, '_' | '.' | '-' | '[' | ']' | '%' | '+')
                    })
            }
            None => false,
        })
}

/// Mask sensitive values in urlencoded `key=value` pairs (query strings and form bodies). A pair
/// without `=` is kept.
pub fn mask_form(text: &str) -> String {
    text.split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) if is_sensitive_key(&percent_decode(k)) => {
                format!("{k}={}", mask_str(v))
            }
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Same as [`mask_form`], for a query string.
pub fn mask_query(query: &str) -> String {
    mask_form(query)
}

/// Mask secrets inside a free text: embedded JSON (optionally behind digits, as in the Engine.IO
/// frame `42["ClientMessage","{\"token\":\"abc\"}"]`) or urlencoded pairs. Other text is returned
/// unchanged.
pub fn mask_text(s: &str) -> String {
    let trimmed = s.trim_start();
    let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    let (prefix, rest) = trimmed.split_at(digits);
    if rest.starts_with(['{', '[', '"'])
        && let Ok(parsed) = serde_json::from_str::<Value>(rest)
    {
        let masked = mask_json(&parsed);
        if masked != parsed {
            return format!("{prefix}{masked}");
        }
        return s.to_string();
    }
    if looks_like_form(s) {
        return mask_form(s);
    }
    s.to_string()
}

/// Mask header values by name (case-insensitive), cut every value at [`MAX_FIELD_BYTES`].
pub fn mask_headers(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(k, v)| {
            let v = if is_sensitive_header(k) {
                mask_str(v)
            } else {
                cap_text(v, MAX_FIELD_BYTES)
            };
            (k.clone(), v)
        })
        .collect()
}

/// Cut a text at `max` bytes (on a char boundary) and append a note.
pub fn cap_text(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... <cut: {} of {} bytes kept>", &s[..end], end, s.len())
}

/// Cut a body larger than [`MAX_BODY_BYTES`]: it becomes a string holding a prefix and a note.
/// Mask before cutting so the prefix cannot leak a secret.
pub fn cut_body(value: &Value) -> Value {
    let text = value.to_string();
    if text.len() <= MAX_BODY_BYTES {
        return value.clone();
    }
    Value::String(cap_text(&text, MAX_BODY_BYTES))
}
