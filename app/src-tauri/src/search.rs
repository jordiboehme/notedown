//! The search in the text of the notes (spec 3).
//!
//! The text store of the pipeline is only on this Mac, and the app cannot link the pipeline
//! crate, so the bundled sidecar searches: `notedown pipeline search` prints the blocks that
//! hold every word of the query. The app keeps the first hit of each note that its notes index
//! lists, so a deleted, unscoped or gone note is never a result.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::AppConfig;
use crate::notes::{NoteSummary, Notes};
use crate::process::{self, SpawnFn};

/// The most blocks the sidecar gives. The app keeps one per note, so this is more than the
/// notes a search shows.
pub const SIDECAR_LIMIT: usize = 1000;

/// How long one search may take before it is stopped.
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);

/// The shortest query (in characters, without the spaces around it) that searches the text.
pub const MIN_CHARS: usize = 2;

/// What the window says when the search did not work.
pub const NOT_SEARCHED: &str = "Notedown could not search the text of the notes.";

/// One block of the sidecar's answer, as `pipeline search` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RawHit {
    /// `notedown://<slug>/<key>/<pageId>#<blockId>`.
    pub anchor: String,
    /// The title of the note.
    pub note: String,
    /// The page number, from 1.
    pub page: usize,
    /// The first line of the block that holds a word of the query.
    pub context: String,
}

/// One note that holds the query, at its first block that does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentHit {
    pub key: String,
    /// The wire path, as the library's entries have it.
    pub path: String,
    pub title: String,
    /// The page number, from 1.
    pub page: usize,
    pub page_id: String,
    pub block_id: String,
    pub context: String,
}

/// True when `q` has a letter or a digit: the pipeline's search splits words at every other
/// character, so a query without one has no words.
pub fn has_word(q: &str) -> bool {
    q.chars().any(char::is_alphanumeric)
}

/// The key, the page id and the block id of `notedown://<slug>/<key>/<pageId>#<blockId>`. The
/// block id is after the last `#`, since a page id can hold one (`P1#2`).
pub fn parse_anchor(a: &str) -> Option<(String, String, String)> {
    let rest = a.strip_prefix("notedown://")?;
    let (place, block) = rest.rsplit_once('#')?;
    let mut parts = place.splitn(3, '/');
    let slug = parts.next()?;
    let key = parts.next()?;
    let page = parts.next()?;
    if [slug, key, page, block].iter().any(|p| p.is_empty()) || key.contains('#') {
        return None;
    }
    Some((key.to_owned(), page.to_owned(), block.to_owned()))
}

/// True when the wire path `path` is the folder `under` or below it. An empty folder is the
/// whole library.
fn is_under(path: &str, under: &str) -> bool {
    let under = under.trim_end_matches('/');
    under.is_empty()
        || path == under
        || path
            .strip_prefix(under)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The first hit of each note in the sidecar's order, for the notes the index lists and, with
/// `under`, only those at or below that folder.
pub fn content_hits(hits: Vec<RawHit>, notes: &Notes, under: Option<&str>) -> Vec<ContentHit> {
    let mut listed: HashMap<String, Option<NoteSummary>> = HashMap::new();
    let mut done: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for hit in hits {
        let Some((key, page_id, block_id)) = parse_anchor(&hit.anchor) else {
            continue;
        };
        if done.contains(&key) {
            continue;
        }
        let summary = listed
            .entry(key.clone())
            .or_insert_with(|| notes.listed_by_key(&key));
        let Some(summary) = summary else {
            continue;
        };
        if under.is_some_and(|u| !is_under(&summary.path, u)) {
            continue;
        }
        done.insert(key.clone());
        out.push(ContentHit {
            key,
            path: summary.path.clone(),
            title: summary.title.clone(),
            page: hit.page,
            page_id,
            block_id,
            context: hit.context,
        });
    }
    out
}

/// True when `query` searches the text: [`MIN_CHARS`] or more and a word.
pub fn wants_text(query: &str) -> bool {
    query.trim().chars().count() >= MIN_CHARS && has_word(query)
}

/// The arguments of `pipeline search`: `--account` only when the config names one, as
/// [`crate::poller::pipeline_status_args`] does.
pub fn search_args(config: &AppConfig, query: &str) -> Vec<String> {
    let mut args = vec![
        "pipeline".to_owned(),
        "search".to_owned(),
        "--data".to_owned(),
        config.data_dir.to_string_lossy().into_owned(),
    ];
    if let Some(account) = config.account.as_deref().filter(|a| !a.is_empty()) {
        args.extend(["--account".to_owned(), account.to_owned()]);
    }
    args.extend([
        "--query".to_owned(),
        query.to_owned(),
        "--limit".to_owned(),
        SIDECAR_LIMIT.to_string(),
    ]);
    args
}

/// The notes whose text holds `query`, by [`content_hits`]. A query that does not search the
/// text ([`wants_text`]) gives nothing and runs no sidecar. Blocking.
pub fn notes_search(
    spawn: &SpawnFn,
    config: &AppConfig,
    notes: &Notes,
    query: &str,
    under: Option<&str>,
) -> Result<Vec<ContentHit>, String> {
    if !wants_text(query) {
        return Ok(Vec::new());
    }
    let output = process::run_bounded(spawn, &search_args(config, query), SEARCH_TIMEOUT)
        .map_err(|why| format!("{NOT_SEARCHED} {why}"))?;
    if !output.status.success() {
        return Err(format!(
            "{NOT_SEARCHED} {}",
            process::error_text(&output.stderr, output.status)
        ));
    }
    let hits: Vec<RawHit> =
        serde_json::from_slice(&output.stdout).map_err(|_| NOT_SEARCHED.to_owned())?;
    Ok(content_hits(hits, notes, under))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;
    use crate::config::Mode;

    const ACCOUNT: &str = "someone";

    struct Store {
        notes: Notes,
        data: tempfile::TempDir,
        _config: tempfile::TempDir,
    }

    impl Store {
        fn new() -> Self {
            let data = tempfile::tempdir().unwrap();
            let config = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(Self::dir(data.path())).unwrap();
            let root = data.path().to_path_buf();
            let notes = Notes::new(
                Arc::new(move || (root.clone(), Some(ACCOUNT.to_owned()))),
                config.path().to_path_buf(),
                Arc::new(|_| {}),
            );
            Self {
                notes,
                data,
                _config: config,
            }
        }

        fn dir(data: &Path) -> PathBuf {
            data.join("pipeline").join(ACCOUNT).join("notes")
        }

        /// A note with text on one page, as the pipeline writes it.
        fn note(&self, key: &str, path: &str, extra: serde_json::Value) {
            let mut note = json!({
                "key": key,
                "path": path,
                "pages": [{"pageId": "P1", "seq": 3, "blocks": [{"id": "b1", "text": "Hi"}]}],
            });
            for (k, v) in extra.as_object().unwrap() {
                note[k] = v.clone();
            }
            std::fs::write(
                Self::dir(self.data.path()).join(format!("{key}.json")),
                note.to_string(),
            )
            .unwrap();
        }

        /// The index reads the store as it is now.
        fn scanned(self) -> Self {
            self.notes.refresh();
            self
        }
    }

    fn raw(anchor: &str, page: usize, context: &str) -> RawHit {
        RawHit {
            anchor: anchor.to_owned(),
            note: "ignored".to_owned(),
            page,
            context: context.to_owned(),
        }
    }

    fn config() -> AppConfig {
        AppConfig {
            mode: Mode::Local,
            data_dir: PathBuf::from("/nowhere/data"),
            account: Some(ACCOUNT.to_owned()),
            ..AppConfig::default()
        }
    }

    /// A spawn that records the arguments of each call and runs a program that fails.
    fn recording() -> (SpawnFn, Arc<Mutex<Vec<Vec<String>>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let spawn: SpawnFn = Arc::new(move |args: &[String]| {
            seen.lock().unwrap().push(args.to_vec());
            std::process::Command::new("/usr/bin/false")
        });
        (spawn, calls)
    }

    /// A fake `notedown` that prints `stdout` and saves its arguments, one per line.
    fn sidecar(dir: &Path, stdout: &str) -> SpawnFn {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(dir.join("out.json"), stdout).unwrap();
        let path = dir.join("notedown");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do echo \"$a\"; done > '{}'\ncat '{}'\n",
                dir.join("args").display(),
                dir.join("out.json").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        process::spawn_program(path)
    }

    #[test]
    fn a_word_is_a_letter_or_a_digit() {
        assert!(!has_word("--"));
        assert!(!has_word(" .,;! "));
        assert!(has_word("a1"));
        assert!(has_word("ü"));
        assert!(has_word("7"));
    }

    #[test]
    fn an_anchor_gives_key_page_and_block() {
        let it = |k: &str, p: &str, b: &str| Some((k.to_owned(), p.to_owned(), b.to_owned()));
        assert_eq!(parse_anchor("notedown://me/F1/P1#b2"), it("F1", "P1", "b2"));
        // A page id can hold a `#`: the block id is after the last one.
        assert_eq!(
            parse_anchor("notedown://me/path:0a/P1#2#b7"),
            it("path:0a", "P1#2", "b7")
        );
        for bad in [
            "",
            "notedown://me/F1/P1",
            "notedown://me/F1#b1",
            "notedown://me/F1/P1#",
            "notedown:///F1/P1#b1",
            "notedown://me//P1#b1",
            "https://me/F1/P1#b1",
        ] {
            assert_eq!(parse_anchor(bad), None, "{bad}");
        }
    }

    #[test]
    fn one_hit_per_note_the_first_in_the_sidecars_order() {
        let store = Store::new();
        store.note("F1", "NOTE/Note/a.note", json!({}));
        store.note("F2", "NOTE/Note/b.note", json!({}));
        let store = store.scanned();
        let hits = content_hits(
            vec![
                raw("notedown://someone/F2/P1#b1", 1, "first of b"),
                raw("notedown://someone/F1/P1#b1", 1, "first of a"),
                raw("notedown://someone/F2/P1#2#b5", 3, "second of b"),
            ],
            &store.notes,
            None,
        );
        assert_eq!(
            hits,
            [
                ContentHit {
                    key: "F2".into(),
                    path: "NOTE/Note/b.note".into(),
                    title: "b".into(),
                    page: 1,
                    page_id: "P1".into(),
                    block_id: "b1".into(),
                    context: "first of b".into(),
                },
                ContentHit {
                    key: "F1".into(),
                    path: "NOTE/Note/a.note".into(),
                    title: "a".into(),
                    page: 1,
                    page_id: "P1".into(),
                    block_id: "b1".into(),
                    context: "first of a".into(),
                },
            ]
        );
    }

    #[test]
    fn a_note_the_index_does_not_list_is_dropped() {
        let store = Store::new();
        store.note("F1", "NOTE/Note/a.note", json!({}));
        store.note("F2", "NOTE/Note/b.note", json!({"unscoped": true}));
        store.note("F3", "NOTE/Note/c.note", json!({"deleted": true}));
        let store = store.scanned();
        let keys: Vec<String> = content_hits(
            vec![
                raw("notedown://someone/F2/P1#b1", 1, "unscoped"),
                raw("notedown://someone/F3/P1#b1", 1, "deleted"),
                raw("notedown://someone/F9/P1#b1", 1, "gone"),
                raw("notedown://someone/F1/P1#b1", 1, "listed"),
            ],
            &store.notes,
            None,
        )
        .into_iter()
        .map(|h| h.key)
        .collect();
        assert_eq!(keys, ["F1"]);
        assert_eq!(store.notes.listed_by_key("F2"), None);
        assert_eq!(
            store.notes.listed_by_key("F1").map(|n| n.path),
            Some("NOTE/Note/a.note".to_owned())
        );
    }

    #[test]
    fn under_keeps_the_notes_at_or_below_the_folder() {
        let store = Store::new();
        store.note("F1", "NOTE/Note/a.note", json!({}));
        store.note("F2", "NOTE/Note/Work/b.note", json!({}));
        store.note("F3", "NOTE/Notes/c.note", json!({}));
        let store = store.scanned();
        let hits = || {
            vec![
                raw("notedown://someone/F1/P1#b1", 1, ""),
                raw("notedown://someone/F2/P1#b1", 1, ""),
                raw("notedown://someone/F3/P1#b1", 1, ""),
            ]
        };
        let keys = |under| -> Vec<String> {
            content_hits(hits(), &store.notes, under)
                .into_iter()
                .map(|h| h.key)
                .collect()
        };
        assert_eq!(keys(Some("NOTE/Note")), ["F1", "F2"]);
        assert_eq!(keys(Some("NOTE/Note/")), ["F1", "F2"]);
        assert_eq!(keys(Some("NOTE/Note/Work")), ["F2"]);
        assert_eq!(keys(Some("")), ["F1", "F2", "F3"]);
        assert_eq!(keys(None), ["F1", "F2", "F3"]);
    }

    #[test]
    fn a_punctuation_query_runs_no_sidecar() {
        let store = Store::new().scanned();
        let (spawn, calls) = recording();
        for query in ["", "a", " b ", "--", "?!", " .. "] {
            assert_eq!(
                notes_search(&spawn, &config(), &store.notes, query, None),
                Ok(Vec::new()),
                "{query:?}"
            );
        }
        assert!(calls.lock().unwrap().is_empty());
        // A query with a word does call it.
        let _ = notes_search(&spawn, &config(), &store.notes, "ab", None);
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn notes_search_reads_the_sidecar_json() {
        let store = Store::new();
        store.note("F1", "NOTE/Note/a.note", json!({}));
        let store = store.scanned();
        let dir = tempfile::tempdir().unwrap();
        let answer = json!([
            {"anchor": "notedown://someone/F1/P1#b1", "note": "a", "page": 1, "context": "The plan"},
            {"anchor": "notedown://someone/F1/P1#b2", "note": "a", "page": 1, "context": "plan B"},
        ]);
        let spawn = sidecar(dir.path(), &answer.to_string());
        let hits = notes_search(&spawn, &config(), &store.notes, "plan", None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].context, "The plan");
        assert_eq!(hits[0].block_id, "b1");
        let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            [
                "pipeline",
                "search",
                "--data",
                "/nowhere/data",
                "--account",
                "someone",
                "--query",
                "plan",
                "--limit",
                "1000"
            ]
        );
    }

    #[test]
    fn a_failed_search_says_so() {
        let store = Store::new().scanned();
        let dir = tempfile::tempdir().unwrap();
        let spawn = sidecar(dir.path(), "not json");
        assert_eq!(
            notes_search(&spawn, &config(), &store.notes, "plan", None),
            Err(NOT_SEARCHED.to_owned())
        );
        let (spawn, _) = recording();
        let why = notes_search(&spawn, &config(), &store.notes, "plan", None).unwrap_err();
        assert!(why.starts_with(NOT_SEARCHED), "{why}");
    }
}
