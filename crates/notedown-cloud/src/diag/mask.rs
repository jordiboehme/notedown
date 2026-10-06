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
    is_sensitive_key_from(key, Origin::Client)
}

/// Who wrote the data being masked. The bare key `code` is a login code only in data the device
/// sends; in server data (HTTP responses, server-to-device frames) it is a result code such as
/// `"403"` and stays readable. Every other key is masked whatever the origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Request query, request body, device-to-server frames.
    Client,
    /// HTTP responses, server-to-device frames.
    Server,
}

/// [`is_sensitive_key`] for data of the given origin.
pub fn is_sensitive_key_from(key: &str, origin: Origin) -> bool {
    let k = key.to_ascii_lowercase();
    (k == "code" && origin == Origin::Client) || SENSITIVE_PARTS.iter().any(|p| k.contains(p))
}

/// Header names: the key rule plus anything containing `auth`, and `channel` (the device may
/// send `<token>_<serial>_<ms>` there, `docs/sync-protocol.md` 2.5).
pub fn is_sensitive_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    is_sensitive_key(name) || n.contains("auth") || n == "channel"
}

fn is_b64url(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'=')
}

/// The first JWT-shaped substring at or after `from`: `eyJ` and two more base64url parts
/// separated by dots. Returns its byte range.
///
/// Linear in the length of the text: when a candidate fails, the scan goes on at the end of the
/// run it has read, because every later `eyJ` inside that run ends at the same place and fails
/// the same way (`"eyJ".repeat(n)` must not cost n scans of the rest).
fn find_jwt(s: &str, from: usize) -> Option<(usize, usize)> {
    let b = s.as_bytes();
    let run = |mut j: usize| {
        while j < b.len() && is_b64url(b[j]) {
            j += 1;
        }
        j
    };
    let mut pos = from;
    while let Some(i) = s.get(pos..).and_then(|t| t.find("eyJ")) {
        let start = pos + i;
        let end1 = run(start);
        if b.get(end1) != Some(&b'.') {
            pos = end1;
            continue;
        }
        let end2 = run(end1 + 1);
        if end2 == end1 + 1 {
            // Empty second part: the next candidate can only start after the dot.
            pos = end1 + 1;
            continue;
        }
        if b.get(end2) != Some(&b'.') {
            // A candidate inside the second part would end at `end2` without a dot as well.
            pos = end2;
            continue;
        }
        let end3 = run(end2 + 1);
        if end3 > end2 + 1 {
            return Some((start, end3));
        }
        // Empty third part: a candidate inside the second part would fail the same way.
        pos = end2 + 1;
    }
    None
}

/// Whether a text holds something shaped like a JWT: `eyJ` and two more base64url parts
/// separated by dots.
pub fn contains_jwt(s: &str) -> bool {
    find_jwt(s, 0).is_some()
}

/// Replace every JWT-shaped substring by the placeholder, whatever key or context it is in. A
/// token in a field the key rule does not know (`accessKey`, a socket event argument, `channel`
/// as `<jwt>_<serial>_<ms>`) is masked this way.
pub fn mask_jwts(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut copied = 0;
    while let Some((start, end)) = find_jwt(s, copied) {
        out.push_str(&s[copied..start]);
        out.push_str(&placeholder(s[start..end].chars().count()));
        copied = end;
    }
    out.push_str(&s[copied..]);
    out
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
    mask_json_from(value, Origin::Client)
}

/// [`mask_json`] for data of the given origin (see [`Origin`]).
pub fn mask_json_from(value: &Value, origin: Origin) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let key = mask_jwts(k);
                    if is_sensitive_key_from(k, origin) {
                        (key, mask_value(v))
                    } else {
                        (key, mask_json_from(v, origin))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| mask_json_from(v, origin)).collect())
        }
        Value::String(s) => Value::String(mask_text_from(s, origin)),
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
    mask_form_from(text, Origin::Client)
}

fn mask_form_from(text: &str, origin: Origin) -> String {
    text.split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) if is_sensitive_key_from(&percent_decode(k), origin) => {
                format!("{}={}", mask_jwts(k), mask_str(v))
            }
            // A token may be percent-encoded: then the whole value goes.
            Some((k, v)) if !contains_jwt(v) && contains_jwt(&percent_decode(v)) => {
                format!("{}={}", mask_jwts(k), mask_str(v))
            }
            _ => mask_jwts(pair),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Same as [`mask_form`], for a query string.
pub fn mask_query(query: &str) -> String {
    mask_form(query)
}

/// Mask secrets inside a free text: embedded JSON (optionally behind digits, as in the Engine.IO
/// frame `42["ClientMessage","{\"token\":\"abc\"}"]`) or urlencoded pairs. Any other text (cut or
/// broken JSON, a form with a bare flag, free text) is masked by substring (`mask_pairs_from`).
pub fn mask_text(s: &str) -> String {
    mask_text_from(s, Origin::Client)
}

/// [`mask_text`] for data of the given origin (see [`Origin`]). JWT-shaped substrings are
/// masked in every case (see [`mask_jwts`]).
pub fn mask_text_from(s: &str, origin: Origin) -> String {
    let trimmed = s.trim_start();
    let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    let (prefix, rest) = trimmed.split_at(digits);
    if rest.starts_with(['{', '[', '"'])
        && let Ok(parsed) = serde_json::from_str::<Value>(rest)
    {
        let masked = mask_json_from(&parsed, origin);
        // With a duplicate key the parsed form has lost the earlier values, so the raw text
        // could hold a secret the masker never saw: log the parsed, masked form instead.
        if masked != parsed || has_duplicate_keys(rest) {
            return format!("{prefix}{masked}");
        }
        return mask_jwts(s);
    }
    if looks_like_form(s) {
        return mask_jwts(&mask_form_from(s, origin));
    }
    mask_jwts(&mask_pairs_from(s, origin, false))
}

/// Mask a raw JSON text without parsing and re-serializing it, so key order and number
/// spelling stay as sent: sensitive key values by substring (an unquoted value becomes a quoted
/// placeholder) and every JWT-shaped substring. `None` (the raw text is left out rather than
/// risking a secret) when
/// - the text has a duplicate key at any depth, also inside embedded JSON strings (the parsed
///   form keeps only the last value, so the checks below cannot see the earlier ones),
/// - the result is not valid JSON or the structural masker ([`mask_json_from`]) still finds
///   something to mask in it,
/// - a string value that the structural masker masks in the parsed text still appears in it.
pub fn mask_raw_json_from(s: &str, origin: Origin) -> Option<String> {
    if has_duplicate_keys(s) {
        return None;
    }
    let original = serde_json::from_str::<Value>(s).ok()?;
    let masked = mask_jwts(&mask_pairs_from(s, origin, true));
    let parsed = serde_json::from_str::<Value>(&masked).ok()?;
    if mask_json_from(&parsed, origin) != parsed {
        return None;
    }
    let mut secrets = Vec::new();
    masked_strings(&original, origin, &mut secrets);
    secrets
        .iter()
        .all(|v| !masked.contains(&Value::String(v.clone()).to_string()))
        .then_some(masked)
}

/// The non-empty string values under sensitive keys (at any depth) that the masker replaces.
fn masked_strings(v: &Value, origin: Origin, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, v) in map {
                match v {
                    Value::String(s)
                        if is_sensitive_key_from(k, origin) && !s.is_empty() && !is_masked(s) =>
                    {
                        out.push(s.clone())
                    }
                    other => masked_strings(other, origin, out),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| masked_strings(v, origin, out)),
        _ => {}
    }
}

const DUPLICATE_KEY: &str = "duplicate key";

/// Whether a JSON text has an object with the same (decoded) key twice, at any depth and inside
/// string values that hold JSON (after an optional numeric Engine.IO prefix). Text that is not
/// JSON has no duplicate keys.
pub fn has_duplicate_keys(s: &str) -> bool {
    matches!(
        serde_json::from_str::<DupCheck>(s),
        Err(e) if e.to_string().starts_with(DUPLICATE_KEY)
    )
}

/// Deserializes any JSON value and fails with [`DUPLICATE_KEY`] on a repeated key.
struct DupCheck;

impl<'de> serde::Deserialize<'de> for DupCheck {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(DupVisitor)
    }
}

struct DupVisitor;

impl<'de> serde::de::Visitor<'de> for DupVisitor {
    type Value = DupCheck;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_bool<E>(self, _: bool) -> Result<DupCheck, E> {
        Ok(DupCheck)
    }
    fn visit_i64<E>(self, _: i64) -> Result<DupCheck, E> {
        Ok(DupCheck)
    }
    fn visit_u64<E>(self, _: u64) -> Result<DupCheck, E> {
        Ok(DupCheck)
    }
    fn visit_f64<E>(self, _: f64) -> Result<DupCheck, E> {
        Ok(DupCheck)
    }
    fn visit_unit<E>(self) -> Result<DupCheck, E> {
        Ok(DupCheck)
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<DupCheck, E> {
        let t = v.trim_start();
        let rest = t.trim_start_matches(|c: char| c.is_ascii_digit());
        if rest.starts_with(['{', '[', '"']) && has_duplicate_keys(rest) {
            return Err(E::custom(DUPLICATE_KEY));
        }
        Ok(DupCheck)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<DupCheck, A::Error> {
        while seq.next_element::<DupCheck>()?.is_some() {}
        Ok(DupCheck)
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<DupCheck, A::Error> {
        let mut seen = std::collections::HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key) {
                return Err(serde::de::Error::custom(DUPLICATE_KEY));
            }
            map.next_value::<DupCheck>()?;
        }
        Ok(DupCheck)
    }
}

fn is_key_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')
}

/// Mask by substring: after every sensitive key name (see [`is_sensitive_key`]) written as
/// `"key": value`, `\"key\":\"value\"`, `'key': value`, `key: value` or `key=value`, the value
/// up to its closing quote (or the end of the text when the quote is missing) or, unquoted, up to
/// the next `,`, `}`, `]`, `&`, `;`, quote or whitespace is replaced by the placeholder.
///
/// With `quote_bare`, an unquoted value is replaced by a quoted placeholder, so JSON stays JSON.
fn mask_pairs_from(s: &str, origin: Origin, quote_bare: bool) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut copied = 0;
    let mut i = 0;
    let skip_ws = |mut j: usize| {
        while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
            j += 1;
        }
        j
    };
    while i < b.len() {
        if !is_key_byte(b[i]) || (i > 0 && is_key_byte(b[i - 1])) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && is_key_byte(b[i]) {
            i += 1;
        }
        let key = &s[start..i];
        let mut j = i;
        if b.get(j) == Some(&b'\\') && b.get(j + 1) == Some(&b'"') {
            j += 2;
        } else if matches!(b.get(j), Some(b'"' | b'\'')) {
            j += 1;
        }
        j = skip_ws(j);
        if !matches!(b.get(j), Some(b':' | b'=')) || !is_sensitive_key_from(key, origin) {
            continue;
        }
        j = skip_ws(j + 1);
        let quote: Option<&str> = if b.get(j) == Some(&b'\\') && b.get(j + 1) == Some(&b'"') {
            Some("\\\"")
        } else if b.get(j) == Some(&b'"') {
            Some("\"")
        } else if b.get(j) == Some(&b'\'') {
            Some("'")
        } else {
            None
        };
        let (vstart, vend, next) = match quote {
            Some(q) => {
                let vstart = j + q.len();
                let end = if q == "\"" {
                    // Skip escaped characters, so `\"` inside the value does not end it.
                    let mut k = vstart;
                    loop {
                        match b.get(k) {
                            None => break None,
                            Some(b'\\') => k += 2,
                            Some(b'"') => break Some(k - vstart),
                            Some(_) => k += 1,
                        }
                    }
                } else {
                    s[vstart..].find(q)
                };
                match end {
                    Some(k) => (vstart, vstart + k, vstart + k + q.len()),
                    None => (vstart, s.len(), s.len()),
                }
            }
            None => {
                let vend = s[j..]
                    .find(|c: char| {
                        matches!(c, ',' | '}' | ']' | '&' | ';' | '"' | '\'' | '\\')
                            || c.is_whitespace()
                    })
                    .map_or(s.len(), |k| j + k);
                (j, vend, vend)
            }
        };
        let value = &s[vstart..vend];
        if !is_masked(value) {
            out.push_str(&s[copied..vstart]);
            if quote.is_none() && quote_bare {
                out.push('"');
                out.push_str(&placeholder(value.chars().count()));
                out.push('"');
            } else {
                out.push_str(&placeholder(value.chars().count()));
            }
            copied = vend;
        }
        i = next.max(i);
    }
    out.push_str(&s[copied..]);
    out
}

/// Mask header values by name (case-insensitive), cut every value at [`MAX_FIELD_BYTES`].
pub fn mask_headers(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(k, v)| {
            let v = if is_sensitive_header(k) || contains_jwt(v) {
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
