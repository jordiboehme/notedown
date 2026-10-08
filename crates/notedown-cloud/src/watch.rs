//! The watch of the data folder: files changed by hand are scanned a short quiet time after the
//! last change, so they reach the device within seconds. The timer scan stays as a backstop.

use std::collections::{BTreeMap, HashSet};
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

/// The times of the watch (tests use shorter ones).
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// How long no new event may arrive before the changed folders are scanned.
    pub quiet: Duration,
    /// How often a missing `<data>/accounts` is looked for again.
    pub missing: Duration,
}

impl Timing {
    pub const DEFAULT: Timing = Timing {
        quiet: QUIET,
        missing: Duration::from_secs(30),
    };
}

/// What a watch scan covers in one account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// The whole tree.
    Whole,
    /// This folder, recursive: the lowest folder that holds all changed paths. One scan sees a
    /// file moved between two of its subfolders as a move and keeps its id.
    Folder(WirePath),
}

/// The scope of the changed `paths` per account: the lowest folder that holds all of them (the
/// whole tree when that is the root). `accounts_dir` is the canonical `<data>/accounts`. With
/// `rescan` (events were lost), every account named by a path is scanned whole.
pub fn scopes_of(accounts_dir: &Path, paths: &[PathBuf], rescan: bool) -> BTreeMap<String, Scope> {
    let mut out = BTreeMap::new();
    for path in paths {
        if let Some((slug, scope)) = scope_of(accounts_dir, path, rescan) {
            merge(&mut out, slug, scope);
        }
    }
    out
}

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
        // A name the wire can't carry: the timer scan reports it.
        Some((_, parent)) if !parent.is_empty() => {
            Scope::Folder(WirePath::parse_named(&parent.join("/")).ok()?)
        }
        _ => Scope::Whole,
    };
    Some((slug.to_string(), scope))
}

/// Add `scope` to the scope of `slug` in `map`: two folders give their lowest common folder,
/// the root gives [`Scope::Whole`].
fn merge(map: &mut BTreeMap<String, Scope>, slug: String, scope: Scope) {
    let merged = match (map.remove(&slug), scope) {
        (None, s) => s,
        (Some(Scope::Folder(a)), Scope::Folder(b)) => {
            let common = a
                .segments()
                .iter()
                .zip(b.segments())
                .take_while(|(x, y)| x == y)
                .count();
            if common == 0 {
                Scope::Whole
            } else {
                // Both are valid paths, so their common prefix is one too.
                match WirePath::parse_named(&a.segments()[..common].join("/")) {
                    Ok(folder) => Scope::Folder(folder),
                    Err(_) => Scope::Whole,
                }
            }
        }
        _ => Scope::Whole,
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

/// Why the watch could not start.
enum SetupError {
    /// `<data>/accounts` does not exist (yet).
    Missing,
    Failed(String),
}

/// Start a recursive watch on `accounts` that sends its events to `tx`; the canonical path of
/// `accounts` and the watcher. Blocking: a recursive watch on Linux walks the whole tree.
fn setup(
    accounts: &Path,
    tx: tokio::sync::mpsc::UnboundedSender<notify::Result<notify::Event>>,
) -> Result<(PathBuf, notify::RecommendedWatcher), SetupError> {
    let failed = |e: &dyn std::fmt::Display| SetupError::Failed(e.to_string());
    // File systems report canonical paths (`/private/var` on macOS for `/var`).
    let dir = std::fs::canonicalize(accounts).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => SetupError::Missing,
        _ => failed(&e),
    })?;
    // The callback runs on notify's thread; after an abort the receiver is gone.
    let mut watcher = notify::recommended_watcher(move |r| {
        let _ = tx.send(r);
    })
    .map_err(|e| failed(&e))?;
    watcher
        .watch(&dir, RecursiveMode::Recursive)
        .map_err(|e| failed(&e))?;
    Ok((dir, watcher))
}

/// Watch `<data>/accounts` and scan the changed folders of each account after `timing.quiet`
/// without a new event ([`AppState::scan_account`]). The scopes of an account whose sync lock
/// is held are kept for the next round, which starts after the next quiet time, at the latest
/// after 5 s. While `<data>/accounts` does not exist, it is looked for every `timing.missing`;
/// once it appears, every account is scanned whole once and the watch starts. Runs until
/// aborted; returns at once (with a warning) when the watch can't start.
pub async fn run(state: Arc<AppState>, timing: Timing) {
    let quiet = timing.quiet;
    let off = |e: &dyn std::fmt::Display| {
        eprintln!("notedown cloud: watch: {e}, hand changes wait for the timer scan");
    };
    let accounts = state.data_dir.join("accounts");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut late = false;
    // The watcher lives as long as this function, so the channel stays open.
    let (dir, _watcher) = loop {
        let (path, tx) = (accounts.clone(), tx.clone());
        match tokio::task::spawn_blocking(move || setup(&path, tx)).await {
            Ok(Ok(w)) => break w,
            Ok(Err(SetupError::Missing)) => {
                if !late {
                    eprintln!(
                        "notedown cloud: watch: {} does not exist yet, the watch starts when \
                         it appears",
                        accounts.display()
                    );
                }
                late = true;
                tokio::time::sleep(timing.missing).await;
            }
            Ok(Err(SetupError::Failed(e))) => return off(&e),
            Err(e) => return off(&e),
        }
    };
    drop(tx);
    let mut batch = Batch {
        // Files may have come in before the watch started.
        all: late,
        ..Batch::default()
    };
    let mut due: Option<Instant> = late.then(|| Instant::now() + quiet);
    let mut busy: BTreeMap<String, Scope> = BTreeMap::new();
    let mut reported = HashSet::new();
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
            Scope::Folder(f) => vec![f.to_string()],
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
    fn a_folder_and_one_below_it_scan_the_upper_one_and_accounts_stay_apart() {
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
    fn sibling_folders_scan_their_parent() {
        let s = scopes_of(
            &acc(),
            &[
                p("me/files/NOTE/Note/A/a.note"),
                p("me/files/NOTE/Note/B/a.note"),
                p("me/files/NOTE/Note/B/C/b.note"),
            ],
            false,
        );
        assert_eq!(folders(&s["me"]), ["NOTE/Note"]);
    }

    #[test]
    fn folders_without_a_common_folder_scan_the_whole_tree() {
        let s = scopes_of(
            &acc(),
            &[
                p("me/files/DOCUMENT/Document/a.pdf"),
                p("me/files/NOTE/Note/a.pdf"),
            ],
            false,
        );
        assert!(matches!(s["me"], Scope::Whole));
    }

    #[test]
    fn a_rescan_event_scans_its_account_whole() {
        let s = scopes_of(&acc(), &[p("me/files/NOTE/Note/a.note")], true);
        assert!(matches!(s["me"], Scope::Whole));
    }
}
