//! The watch of the data folder: files changed by hand are scanned a short quiet time after the
//! last change, so they reach the device within seconds. The timer scan stays as a backstop.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};
use tokio::time::Instant;

use crate::server::{AppState, ScanOutcome};
use crate::store::WirePath;

/// How long no new event may arrive before the changed folders are scanned.
pub const QUIET: Duration = Duration::from_secs(2);

/// How long the scopes of an account whose sync lock was held wait at most for the next try.
const RETRY: Duration = Duration::from_secs(5);

/// What a watch scan covers in one account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// The whole tree.
    Whole,
    /// These folders, each recursive. None lies below another.
    Folders(BTreeSet<WirePath>),
}

/// The scopes of the changed `paths` per account. `accounts_dir` is the canonical
/// `<data>/accounts`. With `rescan` (events were lost), every account named by a path is
/// scanned whole.
pub fn scopes_of(accounts_dir: &Path, paths: &[PathBuf], rescan: bool) -> BTreeMap<String, Scope> {
    let mut out = BTreeMap::new();
    for path in paths {
        if let Some((slug, scope)) = scope_of(accounts_dir, path, rescan) {
            merge(&mut out, slug, scope);
        }
    }
    out
}

/// More folders than this in one account give one whole-tree scan.
const MAX_FOLDERS: usize = 64;

/// The account and scope of one changed path; `None` for a path the watch ignores.
fn scope_of(accounts_dir: &Path, path: &Path, rescan: bool) -> Option<(String, Scope)> {
    let mut parts = path.strip_prefix(accounts_dir).ok()?.components();
    let slug = parts.next()?.as_os_str().to_str()?;
    if slug.starts_with('.') {
        return None;
    }
    if rescan {
        return Some((slug.to_string(), Scope::Whole));
    }
    if parts.next()?.as_os_str() != crate::store::FILES_DIR {
        return None;
    }
    let mut names = Vec::new();
    for part in parts {
        let name = part.as_os_str().to_str()?;
        if name.starts_with('.') {
            return None;
        }
        names.push(name);
    }
    let scope = match names.split_last() {
        Some((_, parent)) if !parent.is_empty() => {
            // A name the wire can't carry: the timer scan reports it.
            let folder = WirePath::parse_named(&parent.join("/")).ok()?;
            Scope::Folders(BTreeSet::from([folder]))
        }
        _ => Scope::Whole,
    };
    Some((slug.to_string(), scope))
}

/// Add `scope` to the scopes of `slug` in `map`: a folder below another one is dropped, and more
/// than [`MAX_FOLDERS`] folders become [`Scope::Whole`].
fn merge(map: &mut BTreeMap<String, Scope>, slug: String, scope: Scope) {
    let merged = match (map.remove(&slug), scope) {
        (None, s) => s,
        (Some(Scope::Whole), _) | (_, Scope::Whole) => Scope::Whole,
        (Some(Scope::Folders(mut a)), Scope::Folders(b)) => {
            a.extend(b);
            Scope::Folders(a)
        }
    };
    let merged = match merged {
        Scope::Folders(all) => {
            let top: BTreeSet<WirePath> = all
                .iter()
                .filter(|f| !all.iter().any(|g| g != *f && f.starts_with(g)))
                .cloned()
                .collect();
            if top.len() > MAX_FOLDERS {
                Scope::Whole
            } else {
                Scope::Folders(top)
            }
        }
        whole => whole,
    };
    map.insert(slug, merged);
}

/// The changes collected since the last round.
#[derive(Default)]
struct Batch {
    scopes: BTreeMap<String, Scope>,
    /// Events were lost and no account can be named: every account is scanned whole.
    all: bool,
}

/// Watch `<data>/accounts` and scan the changed folders of each account after `quiet` without
/// a new event ([`AppState::scan_account`]). The scopes of an account whose sync lock is held
/// are kept for the next round, which starts after the next quiet time, at the latest after
/// 5 s. Runs until aborted; returns at once (with a warning) when the watch can't start.
pub async fn run(state: Arc<AppState>, quiet: Duration) {
    let off = |e: &dyn std::fmt::Display| {
        eprintln!("notedown cloud: watch: {e}, hand changes wait for the timer scan");
    };
    // File systems report canonical paths (`/private/var` on macOS for `/var`).
    let dir = match std::fs::canonicalize(state.data_dir.join("accounts")) {
        Ok(d) => d,
        Err(e) => return off(&e),
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    // The callback runs on notify's thread; after an abort the receiver is gone.
    let mut watcher = match notify::recommended_watcher(move |r| {
        let _ = tx.send(r);
    }) {
        Ok(w) => w,
        Err(e) => return off(&e),
    };
    if let Err(e) = watcher.watch(&dir, RecursiveMode::Recursive) {
        return off(&e);
    }
    let mut batch = Batch::default();
    let mut busy: BTreeMap<String, Scope> = BTreeMap::new();
    let mut reported = HashSet::new();
    let mut due: Option<Instant> = None;
    loop {
        let received = match due {
            None => rx.recv().await,
            Some(at) => tokio::select! {
                r = rx.recv() => r,
                () = tokio::time::sleep_until(at) => {
                    busy = round(&state, std::mem::take(&mut batch), std::mem::take(&mut busy))
                        .await;
                    due = (!busy.is_empty()).then(|| Instant::now() + RETRY);
                    continue;
                }
            },
        };
        // The watcher lives as long as this loop, so the channel never closes here.
        let Some(received) = received else { return };
        match received {
            Ok(event) if event.need_rescan() => {
                let scopes = scopes_of(&dir, &event.paths, true);
                batch.all |= scopes.is_empty();
                for (slug, scope) in scopes {
                    merge(&mut batch.scopes, slug, scope);
                }
            }
            Ok(event) => {
                for (slug, scope) in scopes_of(&dir, &event.paths, false) {
                    merge(&mut batch.scopes, slug, scope);
                }
            }
            Err(e) => {
                if reported.insert(std::mem::discriminant(&e.kind)) {
                    eprintln!("notedown cloud: watch: {e}");
                }
                batch.all = true;
            }
        }
        if batch.all || !batch.scopes.is_empty() || !busy.is_empty() {
            due = Some(Instant::now() + quiet);
        }
    }
}

/// Scan the scopes of `batch` and `busy`, off the async threads; the scopes of the accounts
/// that were busy.
async fn round(
    state: &Arc<AppState>,
    batch: Batch,
    busy: BTreeMap<String, Scope>,
) -> BTreeMap<String, Scope> {
    let st = state.clone();
    let done = tokio::task::spawn_blocking(move || {
        let mut scopes = busy;
        for (slug, scope) in batch.scopes {
            merge(&mut scopes, slug, scope);
        }
        if batch.all {
            match st.auth.accounts().list() {
                Ok(accounts) => {
                    for a in accounts {
                        merge(&mut scopes, a.slug, Scope::Whole);
                    }
                }
                Err(e) => eprintln!("notedown cloud: watch: {e}"),
            }
        }
        scopes
            .into_iter()
            .filter(|(slug, scope)| st.scan_account(slug, scope) == ScanOutcome::Busy)
            .collect()
    })
    .await;
    match done {
        Ok(busy) => busy,
        Err(e) => {
            eprintln!("notedown cloud: watch: scan failed: {e}");
            BTreeMap::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc() -> PathBuf {
        PathBuf::from("/data/accounts")
    }
    fn p(rel: &str) -> PathBuf {
        acc().join(rel)
    }
    fn folders(s: &Scope) -> Vec<String> {
        match s {
            Scope::Whole => vec!["<whole>".into()],
            Scope::Folders(f) => f.iter().map(|w| w.to_string()).collect(),
        }
    }

    #[test]
    fn a_changed_file_scans_its_folder() {
        let s = scopes_of(&acc(), &[p("me/files/NOTE/Note/a.note")], false);
        assert_eq!(folders(&s["me"]), ["NOTE/Note"]);
    }

    #[test]
    fn a_top_level_entry_scans_the_whole_tree() {
        let s = scopes_of(&acc(), &[p("me/files/NOTE")], false);
        assert!(matches!(s["me"], Scope::Whole));
    }

    #[test]
    fn dot_names_and_paths_outside_files_are_ignored() {
        let s = scopes_of(
            &acc(),
            &[
                p("me/files/NOTE/Note/.nd-123.tmp"),
                p("me/files/NOTE/.hidden/a.note"),
                p("me/files/.DS_Store"),
                p("me/ids.jsonl"),
                p("me/recycle/x/a.note"),
                PathBuf::from("/elsewhere/me/files/NOTE/a.note"),
            ],
            false,
        );
        assert!(s.is_empty(), "{s:?}");
    }

    #[test]
    fn a_scope_under_another_is_dropped_and_accounts_stay_apart() {
        let s = scopes_of(
            &acc(),
            &[
                p("me/files/NOTE/Note/W/a.note"),
                p("me/files/NOTE/Note/b.note"),
                p("you/files/Document/c.pdf"),
            ],
            false,
        );
        assert_eq!(folders(&s["me"]), ["NOTE/Note"]);
        assert_eq!(folders(&s["you"]), ["Document"]);
    }

    #[test]
    fn more_than_64_folders_scan_the_whole_tree() {
        let paths: Vec<PathBuf> = (0..65)
            .map(|i| p(&format!("me/files/NOTE/F{i}/a.note")))
            .collect();
        let s = scopes_of(&acc(), &paths, false);
        assert!(matches!(s["me"], Scope::Whole));
        let paths: Vec<PathBuf> = (0..64)
            .map(|i| p(&format!("me/files/NOTE/F{i}/a.note")))
            .collect();
        assert!(matches!(
            scopes_of(&acc(), &paths, false)["me"],
            Scope::Folders(_)
        ));
    }

    #[test]
    fn a_rescan_event_scans_its_account_whole() {
        let s = scopes_of(&acc(), &[p("me/files/NOTE/Note/a.note")], true);
        assert!(matches!(s["me"], Scope::Whole));
    }
}
