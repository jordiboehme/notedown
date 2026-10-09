//! The final check of a safe diagnose bundle (spec 3.4 and 8, point 4): before anything is
//! written, every value is searched for the real names the filter must have replaced. Whole
//! values and whole `/` segments are compared, also inside strings that hold JSON; substrings
//! are not, so a short name such as `a` does not match inside other words.

use std::collections::HashSet;

use serde_json::Value;

use super::filter::is_vocab_key;
use super::standin::FIXED_FOLDERS;

/// The real values a safe bundle must not show.
#[derive(Debug, Default, Clone)]
pub struct Needles {
    set: HashSet<String>,
}

impl Needles {
    pub fn new() -> Needles {
        Needles::default()
    }

    /// Add one value. Values that the bundle shows anyway are not searched: digits only, an MD5,
    /// `Y`, `N`, a fixed folder and the empty string.
    pub fn add(&mut self, s: &str) {
        if !is_exempt(s) {
            self.set.insert(s.to_string());
        }
    }

    /// Add every segment of a `/` path.
    pub fn add_path(&mut self, p: &str) {
        for seg in p.split('/') {
            self.add(seg);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    pub fn len(&self) -> usize {
        self.set.len()
    }

    fn hit(&self, s: &str) -> bool {
        self.set.contains(s) || (s.contains('/') && s.split('/').any(|seg| self.set.contains(seg)))
    }
}

fn is_exempt(s: &str) -> bool {
    s.is_empty()
        || s == "Y"
        || s == "N"
        || FIXED_FOLDERS.contains(&s)
        || s.bytes().all(|b| b.is_ascii_digit())
        || (s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Where a real value was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leak {
    /// The bundle file.
    pub file: String,
    /// The JSON key (or `<key>` for an object key, `<value>` for a value without a key).
    pub key: String,
}

/// Search one JSON value of bundle file `file`. With `route`, the top-level `path` is a route
/// of a protocol log line: only the whole value is compared, not its segments.
pub fn check_json(needles: &Needles, file: &str, v: &Value, route: bool) -> Option<Leak> {
    if needles.is_empty() {
        return None;
    }
    let leak = |key: &str| Leak {
        file: file.to_string(),
        key: key.to_string(),
    };
    if route && let Value::Object(map) = v {
        for (k, item) in map {
            if k == "path"
                && let Value::String(s) = item
            {
                let route = s.split_once('?').map_or(s.as_str(), |(r, _)| r);
                if needles.set.contains(route) {
                    return Some(leak(k));
                }
                continue;
            }
            if needles.set.contains(k) {
                return Some(leak("<key>"));
            }
            if let Some(key) = walk(needles, k, item) {
                return Some(leak(&key));
            }
        }
        return None;
    }
    walk(needles, "<value>", v).map(|key| leak(&key))
}

/// Search loose values (the identifiers written into `report.txt`).
pub fn check_values<'a>(
    needles: &Needles,
    file: &str,
    values: impl IntoIterator<Item = &'a str>,
) -> Option<Leak> {
    for s in values {
        if string_hit(needles, s) {
            return Some(Leak {
                file: file.to_string(),
                key: "<value>".to_string(),
            });
        }
    }
    None
}

/// The key under which a real value was found, if any.
fn walk(needles: &Needles, key: &str, v: &Value) -> Option<String> {
    if is_vocab_key(key) {
        return None;
    }
    match v {
        Value::String(s) => string_hit(needles, s).then(|| key.to_string()),
        Value::Array(items) => items.iter().find_map(|i| walk(needles, key, i)),
        Value::Object(map) => map.iter().find_map(|(k, item)| {
            if needles.set.contains(k) {
                Some("<key>".to_string())
            } else {
                walk(needles, k, item)
            }
        }),
        _ => None,
    }
}

/// A whole value or segment, or a hit inside JSON held by the string (after an optional
/// numeric socket.io prefix).
fn string_hit(needles: &Needles, s: &str) -> bool {
    if needles.hit(s) {
        return true;
    }
    let digits = s.chars().take_while(char::is_ascii_digit).count();
    let rest = &s[digits..];
    if rest.starts_with(['{', '['])
        && let Ok(inner) = serde_json::from_str::<Value>(rest)
    {
        return walk(needles, "<value>", &inner).is_some();
    }
    false
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn needles(values: &[&str]) -> Needles {
        let mut n = Needles::new();
        for v in values {
            n.add(v);
        }
        n
    }

    #[test]
    fn a_whole_value_is_found() {
        let n = needles(&["Secret plans.note"]);
        let leak = check_json(
            &n,
            "journal.jsonl",
            &json!({"x": {"title": "Secret plans.note"}}),
            false,
        );
        assert_eq!(
            leak,
            Some(Leak {
                file: "journal.jsonl".into(),
                key: "title".into()
            })
        );
    }

    #[test]
    fn a_segment_is_found() {
        let n = needles(&["Trips"]);
        assert!(check_json(&n, "f", &json!({"path": "NOTE/Note/Trips/a.note"}), false).is_some());
    }

    #[test]
    fn a_substring_is_not_found() {
        let n = needles(&["a", "plans"]);
        assert!(check_json(&n, "f", &json!({"x": "data", "y": "my plans today"}), false).is_none());
    }

    #[test]
    fn json_in_a_string_is_searched() {
        let n = needles(&["nd-push-test"]);
        let frame = r#"42["ServerMessage","{\"originalName\":\"nd-push-test\"}"]"#;
        assert!(check_json(&n, "f", &json!({"response_body": frame}), true).is_some());
    }

    #[test]
    fn vocabulary_values_are_not_checked() {
        let n = needles(&["note", "Upgrade"]);
        let line = json!({"kind": "note", "headers": {"connection": "Upgrade"}});
        assert!(check_json(&n, "f", &line, true).is_none());
        assert!(check_json(&n, "f", &json!({"title": "note"}), false).is_some());
    }

    #[test]
    fn the_route_is_compared_whole() {
        let n = needles(&["files", "api"]);
        let line = json!({"path": "/api/file/2/files/list_folder?x=1"});
        assert!(check_json(&n, "f", &line, true).is_none());
        assert!(check_json(&n, "f", &line, false).is_some());
    }

    #[test]
    fn digits_md5_and_flags_are_not_needles() {
        let n = needles(&["2024", "98405d2b143a6c9a0d7d9888cbe7ccd4", "Y", "N", ""]);
        assert!(n.is_empty());
    }

    #[test]
    fn fixed_folders_are_not_needles() {
        let mut n = Needles::new();
        n.add_path("NOTE/Note/x");
        assert_eq!(n.len(), 1);
    }

    #[test]
    fn object_keys_are_searched() {
        let n = needles(&["Secret"]);
        assert_eq!(
            check_json(&n, "f", &json!({"a": {"Secret": 1}}), false).map(|l| l.key),
            Some("<key>".into())
        );
    }

    #[test]
    fn loose_values_are_searched() {
        let n = needles(&["SN1"]);
        assert!(check_values(&n, "report.txt", ["dev-1", "SN1"]).is_some());
        assert!(check_values(&n, "report.txt", ["dev-1"]).is_none());
    }
}
