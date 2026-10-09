//! The filter of a diagnose bundle (spec 3 and 8). Every value of the bundle passes it. The key
//! of a value decides what happens to it, and for the top-level fields of a protocol log line
//! the position decides first (the route `path` is no name).
//!
//! With `private` the filter only masks secrets; everything else stays.

use std::net::{IpAddr, Ipv4Addr};

use serde_json::{Map, Value};

use super::mask::{is_masked, is_sensitive_key, mask_json, placeholder};
use super::standin::StandIns;
use crate::{Error, Result};

/// What the filter does with a value, by its key (the one place that lists the keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Ids, hashes, sizes, times, counts, codes: the value stays.
    Keep,
    /// A fixed set of values written by notedown or the firmware: the value stays, and the
    /// final check does not look at it.
    Vocab,
    /// A file or folder name or a path: each segment gets a stand-in.
    Name,
    /// A device serial: `dev-N`.
    Device,
    /// An account slug: `acct-N`.
    Account,
    /// Free text written by notedown: scrubbed (see [`Filter::text`]).
    Message,
    /// Everything else, also keys the filter does not know: `<text:N chars>`.
    Text,
}

const KEEP: &[&str] = &[
    "id",
    "fileId",
    "pageId",
    "directoryId",
    "seq",
    "ts",
    "opId",
    "md5",
    "prevMd5",
    "md5Hash",
    "content_hash",
    "handwriteMd5",
    "handwriteMD5",
    "size",
    "position",
    "pagesChanged",
    "deletedAt",
    "lastUpdateTime",
    "createTime",
    "updateTime",
    "creationTime",
    "lastModified",
    "lastModifiedTime",
    "timestamp",
    "dueTime",
    "completedTime",
    "sortTime",
    "planerSortTime",
    "taskListId",
    "taskId",
    "uniqueIdentifier",
    "parentUniqueIdentifier",
    "userId",
    "errorCode",
    "success",
    "code",
    "old",
    "new",
    "firmware",
    "version",
    "is_downloadable",
    "isDeleted",
    "synType",
    "flag",
    "recursive",
    "page",
    "maxResults",
    "pageSize",
    "currentPage",
    "used",
    "allocated",
    "totalCapacity",
    "sid",
    "pingInterval",
    "pingTimeout",
    "upgrades",
    "nextSyncToken",
    "at",
    "gone",
    "left",
    "min",
    "sort",
    "sortCompleted",
    "planerSort",
    "created_at_ms",
    "updated_at_ms",
];

const VOCAB: &[&str] = &[
    "type",
    "kind",
    "origin",
    "reason",
    "status",
    "messageType",
    "msgType",
    "fileType",
    "sourceType",
    "tag",
    "shareWay",
    "direction",
    "method",
    "flags",
    "isSummaryGroup",
    "isReminderOn",
    "importance",
    "fileServer",
    "isNormal",
];

const NAME: &[&str] = &[
    "path",
    "oldPath",
    "newPath",
    "originalPath",
    "originalName",
    "newName",
    "fileName",
    "innerName",
    "directory",
    "name",
    "path_display",
    "parent_path",
    "sourcePath",
    "commentHandwriteName",
    "handwriteInnerName",
    // the path below `recycle/` of a deleted file
    "recycle",
];

const DEVICE: &[&str] = &["equipmentNo", "device", "serial"];
const ACCOUNT: &[&str] = &["account", "slug"];
const MESSAGE: &[&str] = &["errorMsg", "message"];

/// Header values that stay as they are (names are lowercase in the log).
pub const KEPT_HEADERS: &[&str] = &[
    "content-type",
    "content-length",
    "user-agent",
    "connection",
    "accept-encoding",
    "version",
    "upgrade",
    "sec-websocket-version",
    "language",
];

/// Query keys whose value stays.
const KEPT_QUERY: &[&str] = &[
    "timestamp",
    "nonce",
    "pathId",
    "EIO",
    "transport",
    "random",
    "sid",
];

/// Socket.io event names that stay.
const EVENTS: &[&str] = &["ServerMessage", "ClientMessage", "ratta_ping"];

/// Device values that are no serial and stay.
pub const NO_SERIAL: &[&str] = &["", "null-null"];

/// The class of a value by its key.
pub fn class_of(key: &str) -> Class {
    if KEEP.contains(&key) {
        Class::Keep
    } else if VOCAB.contains(&key) {
        Class::Vocab
    } else if NAME.contains(&key) {
        Class::Name
    } else if DEVICE.contains(&key) {
        Class::Device
    } else if ACCOUNT.contains(&key) {
        Class::Account
    } else if MESSAGE.contains(&key) {
        Class::Message
    } else {
        Class::Text
    }
}

/// Whether the final check skips the value of this key: a vocabulary value or a header value
/// that stays as it is.
pub fn is_vocab_key(key: &str) -> bool {
    class_of(key) == Class::Vocab || KEPT_HEADERS.contains(&key)
}

/// A hidden text: `<text:N chars>`.
pub fn hidden(s: &str) -> String {
    format!("<text:{} chars>", s.chars().count())
}

/// A placeholder written by notedown that stays: `<masked:N chars>`, `<N bytes, ...>`,
/// `<close 1001 idle>`, `<text:N chars>`.
pub fn is_placeholder(s: &str) -> bool {
    if is_masked(s) {
        return true;
    }
    let Some(inner) = s.strip_prefix('<').and_then(|r| r.strip_suffix('>')) else {
        return false;
    };
    if inner.contains(['<', '>']) {
        return false;
    }
    let digits = inner.chars().take_while(char::is_ascii_digit).count();
    (digits > 0 && inner[digits..].starts_with(" bytes"))
        || inner.starts_with("close ")
        || inner.starts_with("text:")
}

/// The real values used to scrub free text written by notedown (spec 8, point 5).
#[derive(Debug, Clone, Default)]
pub struct Scrub {
    /// The data folder as given and as its real path.
    pub data_dirs: Vec<String>,
    pub home: Option<String>,
    pub slugs: Vec<String>,
    pub serials: Vec<String>,
    /// Account e-mail addresses and user names.
    pub people: Vec<String>,
    /// File and folder name segments.
    pub names: Vec<String>,
}

pub struct Filter {
    pub private: bool,
    pub standins: StandIns,
    scrub: Scrub,
    test_keep: Option<String>,
}

impl Filter {
    pub fn new(private: bool, standins: StandIns, scrub: Scrub) -> Filter {
        Filter {
            private,
            standins,
            scrub,
            test_keep: None,
        }
    }

    /// Test hook: keep the values of `key` as they are, to prove the final check.
    #[doc(hidden)]
    pub fn keep_key_for_test(&mut self, key: &str) {
        self.test_keep = Some(key.to_string());
    }

    pub fn device(&mut self, serial: &str) -> String {
        if self.private || NO_SERIAL.contains(&serial) {
            serial.to_string()
        } else {
            self.standins.device(serial)
        }
    }

    /// `acct-N` for an account slug. Another value under an account key (the login sends the
    /// e-mail address as `account`) is hidden, so `acct-N` always means an account.
    pub fn account(&mut self, slug: &str) -> String {
        if self.private {
            slug.to_string()
        } else if self.scrub.slugs.is_empty() || self.scrub.slugs.iter().any(|s| s == slug) {
            self.standins.account(slug)
        } else {
            hidden(slug)
        }
    }

    pub fn path(&mut self, p: &str) -> String {
        if self.private || is_placeholder(p) {
            p.to_string()
        } else {
            self.standins.path(p)
        }
    }

    /// Scrub free text written by notedown: the data folder becomes `<data>`, the home folder
    /// `~`, then slugs, serials, people and names (longest first, whole words only) get their
    /// stand-ins, and IPv4 addresses too.
    pub fn text(&mut self, s: &str) -> String {
        if self.private {
            return s.to_string();
        }
        let mut out = s.to_string();
        let mut dirs = self.scrub.data_dirs.clone();
        dirs.sort_by_key(|d| std::cmp::Reverse(d.len()));
        for d in dirs.iter().filter(|d| !d.is_empty()) {
            out = out.replace(d.as_str(), "<data>");
        }
        if let Some(home) = self.scrub.home.as_deref().filter(|h| h.len() > 1) {
            out = out.replace(home, "~");
        }
        let mut words: Vec<(String, u8)> = Vec::new();
        words.extend(self.scrub.slugs.iter().map(|w| (w.clone(), 0)));
        words.extend(self.scrub.serials.iter().map(|w| (w.clone(), 1)));
        words.extend(self.scrub.people.iter().map(|w| (w.clone(), 2)));
        words.extend(self.scrub.names.iter().map(|w| (w.clone(), 3)));
        words.retain(|(w, _)| !w.is_empty());
        words.sort_by_key(|(w, _)| std::cmp::Reverse(w.len()));
        for (word, what) in words {
            if !out.contains(word.as_str()) {
                continue;
            }
            let with = match what {
                0 => self.standins.account(&word),
                1 => self.standins.device(&word),
                2 => "<person>".to_string(),
                _ => self.standins.segment(&word),
            };
            out = replace_words(&out, &word, &with);
        }
        self.scrub_ips(&out)
    }

    fn scrub_ips(&mut self, s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut token = String::new();
        let flush = |token: &mut String, out: &mut String, me: &mut Filter| {
            if token.parse::<Ipv4Addr>().is_ok() {
                out.push_str(&me.standins.ip(token));
            } else {
                out.push_str(token);
            }
            token.clear();
        };
        for c in s.chars() {
            if c.is_ascii_digit() || c == '.' {
                token.push(c);
            } else {
                flush(&mut token, &mut out, self);
                out.push(c);
            }
        }
        flush(&mut token, &mut out, self);
        out
    }

    /// A string under a key of `class`.
    fn string(&mut self, class: Class, s: &str) -> String {
        match class {
            Class::Keep | Class::Vocab => s.to_string(),
            Class::Name => self.path(s),
            Class::Device => self.device(s),
            Class::Account => self.account(s),
            Class::Message => self.text(s),
            Class::Text if is_placeholder(s) => s.to_string(),
            Class::Text => hidden(s),
        }
    }

    /// A JSON value by the key rules (bodies, journal lines, nested JSON).
    pub fn json(&mut self, v: &Value) -> Value {
        if self.private {
            return v.clone();
        }
        self.value(Class::Text, v)
    }

    fn value(&mut self, class: Class, v: &Value) -> Value {
        match v {
            Value::Object(map) => Value::Object(self.object(map)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|i| self.value(class, i)).collect())
            }
            Value::String(s) => Value::String(self.string(class, s)),
            other => other.clone(),
        }
    }

    fn object(&mut self, map: &Map<String, Value>) -> Map<String, Value> {
        let mut out = Map::new();
        for (k, v) in map {
            let class = class_of(k);
            let value = if self.test_keep.as_deref() == Some(k.as_str()) {
                v.clone()
            } else if k == "parts" {
                self.parts(v)
            } else {
                self.value(class, v)
            };
            out.insert(k.clone(), value);
        }
        out
    }

    /// `parts` of a page event: `name` is vocabulary (`strokes`, `layers`).
    fn parts(&mut self, v: &Value) -> Value {
        match v {
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|i| match i {
                        Value::Object(m) => Value::Object(
                            m.iter()
                                .map(|(k, v)| {
                                    let class = if k == "name" {
                                        Class::Vocab
                                    } else {
                                        class_of(k)
                                    };
                                    (k.clone(), self.value(class, v))
                                })
                                .collect(),
                        ),
                        other => self.value(Class::Text, other),
                    })
                    .collect(),
            ),
            other => self.value(Class::Text, other),
        }
    }

    /// A body that is a string: a placeholder, a socket.io frame, or JSON text (filtered and
    /// written back as text). Anything else is hidden.
    pub fn string_body(&mut self, s: &str) -> String {
        if self.private || is_placeholder(s) {
            return s.to_string();
        }
        let digits = s.chars().take_while(char::is_ascii_digit).count();
        let (prefix, rest) = s.split_at(digits);
        if rest.is_empty() {
            return s.to_string();
        }
        let Ok(parsed) = serde_json::from_str::<Value>(rest) else {
            return hidden(s);
        };
        let filtered = match (&parsed, prefix) {
            (Value::Array(items), p) if p.starts_with("42") => Value::Array(self.event(items)),
            (Value::Array(items), p) if p.starts_with("43") => Value::Array(self.args(items)),
            _ => self.json(&parsed),
        };
        format!("{prefix}{filtered}")
    }

    /// A socket.io event: the name stays when it is known, then the arguments.
    fn event(&mut self, items: &[Value]) -> Vec<Value> {
        let mut out = Vec::new();
        let mut rest = items;
        if let Some((Value::String(name), tail)) = items.split_first() {
            out.push(Value::String(if EVENTS.contains(&name.as_str()) {
                name.clone()
            } else {
                hidden(name)
            }));
            rest = tail;
        }
        out.extend(self.args(rest));
        out
    }

    /// Socket.io arguments: `Received` stays, JSON in a string is filtered, other text hidden.
    fn args(&mut self, items: &[Value]) -> Vec<Value> {
        items
            .iter()
            .map(|v| match v {
                Value::String(s) if s == "Received" => v.clone(),
                Value::String(s) => match serde_json::from_str::<Value>(s) {
                    Ok(inner @ (Value::Object(_) | Value::Array(_))) => {
                        Value::String(self.json(&inner).to_string())
                    }
                    _ => Value::String(hidden(s)),
                },
                other => self.json(other),
            })
            .collect()
    }

    /// A query string: known keys keep their value, `type` is a serial, masked values stay,
    /// every other value is hidden (also the base64 `path=` of `oss/download`).
    pub fn query(&mut self, q: &str) -> String {
        if self.private {
            return q.to_string();
        }
        q.split('&')
            .map(|pair| match pair.split_once('=') {
                Some((k, v)) if KEPT_QUERY.contains(&k) || is_placeholder(v) => pair.to_string(),
                Some((k, v)) if k == "type" => format!("{k}={}", self.device(v)),
                Some((k, v)) => format!("{k}={}", hidden(v)),
                None => hidden(pair),
            })
            .collect::<Vec<_>>()
            .join("&")
    }

    /// Headers: only the names and the kept values stay; `host` keeps its port.
    pub fn headers(&mut self, h: &Map<String, Value>) -> Map<String, Value> {
        if self.private {
            return h.clone();
        }
        h.iter()
            .map(|(name, v)| {
                let text = v.as_str().unwrap_or_default();
                let out = if KEPT_HEADERS.contains(&name.as_str()) || is_placeholder(text) {
                    text.to_string()
                } else if name == "equipmentno" {
                    self.device(text)
                } else if name == "host" {
                    self.host(text)
                } else {
                    hidden(text)
                };
                (name.clone(), Value::String(out))
            })
            .collect()
    }

    /// `host[:port]`: an IP gets `ip-N`, a host name `<host>`; the port stays.
    fn host(&mut self, text: &str) -> String {
        let (host, port) = split_host_port(text);
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        let name = if bare.parse::<IpAddr>().is_ok() {
            self.standins.ip(bare)
        } else {
            "<host>".to_string()
        };
        match port {
            Some(p) => format!("{name}:{p}"),
            None => name,
        }
    }

    /// One protocol log line (already masked on disk).
    pub fn log_line(&mut self, v: &Value) -> Value {
        if self.private {
            return v.clone();
        }
        let Value::Object(map) = v else {
            return self.json(v);
        };
        let mut out = Map::new();
        for (k, v) in map {
            let s = v.as_str();
            let value = match (k.as_str(), v) {
                (
                    "ts" | "kind" | "method" | "response_status" | "success" | "error_code"
                    | "duration_ms" | "flags" | "direction" | "dropped" | "firmware",
                    _,
                ) => v.clone(),
                ("device", _) => Value::String(self.device(s.unwrap_or_default())),
                ("path", Value::String(p)) => Value::String(match p.split_once('?') {
                    Some((route, q)) => format!("{route}?{}", self.query(q)),
                    None => p.clone(),
                }),
                ("query", Value::String(q)) => Value::String(self.query(q)),
                ("headers", Value::Object(h)) => Value::Object(self.headers(h)),
                ("message", Value::String(m)) => Value::String(self.text(m)),
                (
                    "request_body" | "response_body" | "request_body_raw" | "response_body_raw",
                    Value::String(b),
                ) => Value::String(self.string_body(b)),
                ("request_body" | "response_body", body) => self.json(body),
                (_, other) => self.value(Class::Text, other),
            };
            out.insert(k.clone(), value);
        }
        Value::Object(out)
    }

    /// One journal line: secrets masked, then the key rules.
    pub fn journal_line(&mut self, v: &Value) -> Value {
        let masked = mask_json(v);
        self.json(&masked)
    }

    /// `server.toml`: secrets masked in every mode; in a safe bundle the addresses get
    /// stand-ins, `render_dir` becomes `<dir>`, the host of `external_base_url` `<host>`, and
    /// strings of unknown keys are hidden.
    pub fn server_toml(&mut self, text: &str) -> Result<String> {
        let mut table: toml::Table = toml::from_str(text).map_err(|e| Error::Invalid {
            what: "server.toml",
            message: e.to_string(),
        })?;
        let mut changed = false;
        for (k, v) in table.iter_mut() {
            if is_sensitive_key(k) {
                *v = toml::Value::String(placeholder(v.to_string().chars().count()));
                changed = true;
            }
        }
        if self.private {
            return Ok(if changed {
                toml::to_string(&table).unwrap_or_default()
            } else {
                text.to_string()
            });
        }
        for (k, v) in table.iter_mut() {
            if is_sensitive_key(k) {
                continue;
            }
            *v = match (k.as_str(), &*v) {
                ("listen" | "trusted_proxies", toml::Value::Array(items)) => toml::Value::Array(
                    items
                        .iter()
                        .map(|i| match i {
                            toml::Value::String(s) => toml::Value::String(self.host(s)),
                            other => other.clone(),
                        })
                        .collect(),
                ),
                ("render_dir", toml::Value::String(_)) => toml::Value::String("<dir>".into()),
                ("external_base_url", toml::Value::String(u)) => toml::Value::String(self.url(u)),
                ("sync" | "push", s @ toml::Value::String(_)) => s.clone(),
                (_, toml::Value::String(s)) => toml::Value::String(hidden(s)),
                (_, other) => other.clone(),
            };
        }
        Ok(toml::to_string(&table).unwrap_or_default())
    }

    /// A URL: the scheme and the port stay, the host becomes `ip-N` or `<host>`, a path is
    /// hidden.
    fn url(&mut self, u: &str) -> String {
        let (scheme, rest) = u.split_once("://").unwrap_or(("", u));
        let (authority, path) = match rest.find('/') {
            Some(i) => rest.split_at(i),
            None => (rest, ""),
        };
        let host = self.host(authority.rsplit('@').next().unwrap_or(authority));
        let path = if path.is_empty() || path == "/" {
            path.to_string()
        } else {
            format!("/{}", hidden(path))
        };
        if scheme.is_empty() {
            format!("{host}{path}")
        } else {
            format!("{scheme}://{host}{path}")
        }
    }
}

/// `host:port`, `[v6]:port`, a bare v6 address or a bare host.
fn split_host_port(text: &str) -> (&str, Option<&str>) {
    if let Some(end) = text.find(']') {
        let host = &text[..=end];
        let port = text[end + 1..].strip_prefix(':');
        return (host, port);
    }
    if text.matches(':').count() == 1 {
        let (h, p) = text.split_once(':').expect("one colon");
        if p.chars().all(|c| c.is_ascii_digit()) {
            return (h, Some(p));
        }
    }
    (text, None)
}

/// Replace `word` where it stands as a whole word: no letter or digit right before or after.
fn replace_words(text: &str, word: &str, with: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut before: Option<char> = None;
    while let Some(i) = rest.find(word) {
        let prev = rest[..i].chars().next_back().or(before);
        let next = rest[i + word.len()..].chars().next();
        let bounded =
            !prev.is_some_and(char::is_alphanumeric) && !next.is_some_and(char::is_alphanumeric);
        out.push_str(&rest[..i]);
        if bounded {
            out.push_str(with);
        } else {
            out.push_str(word);
        }
        before = word.chars().next_back();
        rest = &rest[i + word.len()..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn filter() -> Filter {
        Filter::new(false, StandIns::new(4), Scrub::default())
    }

    fn is_stand_in(s: &str) -> bool {
        s.strip_prefix("n-").is_some_and(|r| {
            let hex = r.split('.').next().unwrap();
            hex.len() == 4 && hex.bytes().all(|b| b.is_ascii_hexdigit())
        })
    }

    #[test]
    fn kept_and_vocab_keys_stay() {
        let mut f = filter();
        let v = json!({"id": "1054", "md5": "98405d2b143a6c9a0d7d9888cbe7ccd4", "size": 7,
            "success": true, "errorCode": "E0079", "type": "file.created", "kind": "note",
            "messageType": "ADDFOLDER", "fileType": "FOLDE", "flags": ["x"]});
        assert_eq!(f.json(&v), v);
    }

    #[test]
    fn name_keys_get_stand_ins() {
        let mut f = filter();
        let out = f.json(
            &json!({"path": "/NOTE/Note/Planview/Plainview Overview.note",
            "fileName": "Plainview Overview.note", "path_display": "NOTE/Note/a"}),
        );
        let path = out["path"].as_str().unwrap();
        let parts: Vec<_> = path.split('/').collect();
        assert_eq!(parts[..3], ["", "NOTE", "Note"]);
        assert!(is_stand_in(parts[3]) && is_stand_in(parts[4]), "{path}");
        assert_eq!(out["fileName"], parts[4]);
        assert!(!out.to_string().contains("view"));
        assert!(!out.to_string().contains("/a\""));
    }

    #[test]
    fn an_unknown_key_is_hidden() {
        let mut f = filter();
        let out = f.json(
            &json!({"email": "a@b.c", "userName": "jordi", "content": "text",
            "title": "To do", "newFirmwareField": "x", "list": ["a", "bb"]}),
        );
        assert_eq!(
            out,
            json!({"email": "<text:5 chars>", "userName": "<text:5 chars>",
                "content": "<text:4 chars>", "title": "<text:5 chars>",
                "newFirmwareField": "<text:1 chars>", "list": ["<text:1 chars>", "<text:2 chars>"]})
        );
    }

    #[test]
    fn query_keeps_known_keys() {
        let mut f = filter();
        let q = "path=WyJqb3JkaSJd&signature=<masked:64 chars>&timestamp=1&nonce=ab&pathId=1077\
                 &EIO=3&transport=websocket&random=17&type=SN078D10000042&token=<masked:9 chars>";
        assert_eq!(
            f.query(q),
            "path=<text:12 chars>&signature=<masked:64 chars>&timestamp=1&nonce=ab&pathId=1077\
             &EIO=3&transport=websocket&random=17&type=dev-1&token=<masked:9 chars>"
        );
    }

    #[test]
    fn headers_keep_only_the_allowed_values() {
        let mut f = filter();
        let h = json!({"host": "192.168.1.31:19072", "equipmentno": "SN1", "channel": "<masked:3 chars>",
            "user-agent": "okhttp/5.1.0", "x-other": "abc", "connection": "Upgrade"});
        let out = f.headers(h.as_object().unwrap());
        assert_eq!(
            Value::Object(out),
            json!({"host": "ip-1:19072", "equipmentno": "dev-1", "channel": "<masked:3 chars>",
                "user-agent": "okhttp/5.1.0", "x-other": "<text:3 chars>", "connection": "Upgrade"})
        );
        let h6 = json!({"host": "[fe80::1]:19072", "x": "y"});
        assert_eq!(f.headers(h6.as_object().unwrap())["host"], "ip-2:19072");
        let named = json!({"host": "notes.example.org"});
        assert_eq!(f.headers(named.as_object().unwrap())["host"], "<host>");
    }

    #[test]
    fn json_in_a_frame_string_is_filtered() {
        let mut f = filter();
        let frame = r#"42["ServerMessage","{\"code\":\"200\",\"timestamp\":1791546910120,\"msgType\":\"FILE-SYN\",\"data\":[{\"messageType\":\"ADDFOLDER\",\"equipmentNo\":\"null-null\",\"fileType\":\"FOLDE\",\"id\":1076,\"originalName\":\"nd-push-test\",\"size\":0,\"directoryId\":1001,\"originalPath\":\"NOTE/Note/\",\"timestamp\":1791546909734}]}"]"#;
        let out = f.string_body(frame);
        assert!(out.starts_with(r#"42["ServerMessage",""#), "{out}");
        for kept in [
            "ADDFOLDER",
            "FOLDE",
            "1076",
            "null-null",
            "FILE-SYN",
            "NOTE/Note/",
        ] {
            assert!(out.contains(kept), "{kept} in {out}");
        }
        assert!(!out.contains("nd-push-test"), "{out}");
        for same in [
            r#"42["ratta_ping","Received"]"#,
            r#"421["ClientMessage","Received"]"#,
            "431[]",
            "2",
            "40",
            "<close 1001 idle>",
        ] {
            assert_eq!(f.string_body(same), same);
        }
        assert_eq!(
            f.string_body(r#"42["Other","secret"]"#),
            r#"42["<text:5 chars>","<text:6 chars>"]"#
        );
        let hs = r#"0{"sid":"abc","upgrades":[],"pingInterval":5000,"pingTimeout":25000}"#;
        let out = f.string_body(hs);
        let parsed: Value = serde_json::from_str(out.strip_prefix('0').unwrap()).unwrap();
        assert_eq!(parsed, serde_json::from_str::<Value>(&hs[1..]).unwrap());
    }

    #[test]
    fn an_unparsable_body_is_hidden() {
        let mut f = filter();
        assert_eq!(f.string_body("hello there"), "<text:11 chars>");
        assert_eq!(
            f.string_body("<7369073 bytes, not logged>"),
            "<7369073 bytes, not logged>"
        );
        assert_eq!(
            f.string_body("<186649 bytes streamed>"),
            "<186649 bytes streamed>"
        );
        assert_eq!(f.string_body("<b>x</b>"), "<text:8 chars>");
    }

    #[test]
    fn the_route_path_stays_and_its_query_is_filtered() {
        let mut f = filter();
        let line = json!({"ts": "t", "kind": "request", "device": "SN1", "path": "/api/oss/download?path=abc&timestamp=1",
            "message": "account jordi", "request_body": {"path": "/NOTE/Secret"}, "other": "x"});
        let out = f.log_line(&line);
        assert_eq!(
            out["path"],
            "/api/oss/download?path=<text:3 chars>&timestamp=1"
        );
        assert_eq!(out["device"], "dev-1");
        assert!(is_stand_in(
            out["request_body"]["path"]
                .as_str()
                .unwrap()
                .split('/')
                .nth(2)
                .unwrap()
        ));
        assert_eq!(out["other"], "<text:1 chars>");
    }

    #[test]
    fn journal_lines_replace_names_accounts_and_devices() {
        let mut f = filter();
        let line = json!({"seq": 1, "ts": 2, "account": "jordi", "origin": "device", "device": "SN1",
            "type": "page.modified", "path": "NOTE/Note/Secret.note", "oldPath": "NOTE/Note/Old.note",
            "title": "Buy milk", "parts": [{"name": "strokes", "old": "a", "new": "b"}],
            "token": "abc"});
        let out = f.journal_line(&line);
        assert_eq!(out["account"], "acct-1");
        assert_eq!(out["device"], "dev-1");
        assert_eq!(out["type"], "page.modified");
        assert_eq!(out["title"], "<text:8 chars>");
        assert_eq!(out["parts"][0]["name"], "strokes");
        assert_eq!(out["token"], "<masked:3 chars>");
        let deleted = f.journal_line(&json!({"type": "file.deleted",
            "recycle": "1791466487188/NOTE/Note/Secret.note", "path": "NOTE/Note/Secret.note"}));
        let recycle = deleted["recycle"].as_str().unwrap();
        assert!(
            recycle.starts_with("1791466487188/NOTE/Note/n-"),
            "{recycle}"
        );
        assert_eq!(
            deleted["recycle"].as_str().unwrap().rsplit('/').next(),
            deleted["path"].as_str().unwrap().rsplit('/').next()
        );
        assert!(!out.to_string().contains("Secret") && !out.to_string().contains("Old"));
    }

    #[test]
    fn free_text_is_scrubbed() {
        let scrub = Scrub {
            data_dirs: vec!["/Users/x/data".into()],
            home: Some("/Users/x".into()),
            slugs: vec!["jordi".into()],
            serials: vec!["SN1".into()],
            people: vec!["jordi@example.org".into()],
            names: vec!["Secret.note".into(), "a".into()],
        };
        let mut f = Filter::new(false, StandIns::new(4), scrub);
        let out = f.text(
            "account jordi: recovery failed: /Users/x/data/accounts/jordi/files/NOTE/Note/Secret.note: \
             gone, see /Users/x/log, device SN1 at 10.0.0.7, mail jordi@example.org, a data",
        );
        assert!(
            out.starts_with(
                "account acct-1: recovery failed: <data>/accounts/acct-1/files/NOTE/Note/n-"
            ),
            "{out}"
        );
        assert!(
            out.contains(".note: gone, see ~/log, device dev-1 at ip-1, mail <person>, n-"),
            "{out}"
        );
        assert!(out.ends_with(" data"), "{out}");
    }

    #[test]
    fn an_account_key_that_holds_no_slug_is_hidden() {
        let scrub = Scrub {
            slugs: vec!["jordi".into()],
            ..Scrub::default()
        };
        let mut f = Filter::new(false, StandIns::new(4), scrub);
        let out = f.json(&json!({"account": "jordi@example.org"}));
        assert_eq!(out["account"], "<text:17 chars>");
        assert_eq!(f.account("jordi"), "acct-1");
    }

    #[test]
    fn server_toml_is_filtered() {
        let mut f = filter();
        let text = "listen = [\"0.0.0.0:19072\", \"[::]:19072\"]\ntrusted_proxies = [\"10.0.0.1\"]\n\
                    external_base_url = \"https://notes.example.org:8443/sn\"\nsync = \"accept\"\n\
                    render_dir = \"~/PDFs\"\nprotect = true\nlog_retention_days = 30\n";
        let out: toml::Table = toml::from_str(&f.server_toml(text).unwrap()).unwrap();
        assert_eq!(
            out["listen"].as_array().unwrap()[0].as_str(),
            Some("ip-1:19072")
        );
        assert_eq!(
            out["listen"].as_array().unwrap()[1].as_str(),
            Some("ip-2:19072")
        );
        assert_eq!(
            out["trusted_proxies"].as_array().unwrap()[0].as_str(),
            Some("ip-3")
        );
        assert_eq!(
            out["external_base_url"].as_str(),
            Some("https://<host>:8443/<text:3 chars>")
        );
        assert_eq!(out["render_dir"].as_str(), Some("<dir>"));
        assert_eq!(out["sync"].as_str(), Some("accept"));
        assert_eq!(out["protect"].as_bool(), Some(true));
    }

    #[test]
    fn private_only_masks_secrets() {
        let mut f = Filter::new(true, StandIns::new(4), Scrub::default());
        let line = json!({"account": "jordi", "path": "NOTE/Secret", "token": "abc"});
        let out = f.journal_line(&line);
        assert_eq!(
            out,
            json!({"account": "jordi", "path": "NOTE/Secret", "token": "<masked:3 chars>"})
        );
        let text = "render_dir = \"~/PDFs\"\n";
        assert_eq!(f.server_toml(text).unwrap(), text);
    }

    #[test]
    fn whole_words_only() {
        assert_eq!(replace_words("a data a", "a", "X"), "X data X");
        assert_eq!(replace_words("jordi-x jordix", "jordi", "Y"), "Y-x jordix");
    }
}
