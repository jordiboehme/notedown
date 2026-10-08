//! The `render` consumer: one pass per account at start, then a pass for each account whose
//! notes changed, after a quiet time, so a sync of many notes is one pass.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::{PassOptions, PassReport, render_account};
use crate::journal::{EventBody, FileKind};
use crate::server::AppState;

/// The name of the journal cursor of this consumer.
pub const CONSUMER: &str = "render";

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// How long no note event may arrive before the pass starts.
    pub quiet: Duration,
    /// How often the journal is read.
    pub poll: Duration,
    /// How long an account whose pass failed (also on a note's disk error) waits for the next
    /// try.
    pub retry: Duration,
}

impl Timing {
    pub const DEFAULT: Timing = Timing {
        quiet: Duration::from_secs(2),
        poll: Duration::from_secs(1),
        retry: Duration::from_secs(30),
    };
}

fn is_note(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    FileKind::of(name) == FileKind::Note
}

/// Whether an event can change the PDFs: a file event of a note (by its path's extension),
/// a folder move or delete.
pub fn wakes(body: &EventBody) -> bool {
    match body {
        EventBody::FileCreated { path, .. }
        | EventBody::FileModified { path, .. }
        | EventBody::FileMoved { path, .. }
        | EventBody::FileCopied { path, .. }
        | EventBody::FileDeleted { path, .. } => is_note(path),
        EventBody::FolderMoved { .. } | EventBody::FolderDeleted { .. } => true,
        _ => false,
    }
}

fn pass_line(slug: &str, r: &PassReport) -> Option<String> {
    let (w, m, d, f) = (
        r.written.len(),
        r.moved.len(),
        r.deleted.len(),
        r.failed.len(),
    );
    if w + m + d + f == 0 {
        return None;
    }
    Some(format!(
        "notedown render: {slug}: written {w}, moved {m}, deleted {d}, failed {f}"
    ))
}

/// What a pass came to.
enum Outcome {
    Done,
    /// Another pass held the render lock.
    Busy,
    Failed(String),
}

/// One pass for `slug`, off the async threads.
async fn pass(state: &Arc<AppState>, root: &Path, slug: &str) -> Outcome {
    let st = state.clone();
    let root = root.to_path_buf();
    let name = slug.to_string();
    let done = tokio::task::spawn_blocking(move || {
        let store = st.store(&name)?;
        render_account(
            &store,
            &root,
            &PassOptions {
                all: false,
                wait_for_lock: false,
            },
        )
    })
    .await;
    match done {
        Ok(Ok(report)) if report.busy => Outcome::Busy,
        Ok(Ok(report)) if report.retry.is_some() => {
            // The PDFs that were done still get their line; the disk error is the failure.
            if report.written.len() + report.moved.len() + report.deleted.len() > 0
                && let Some(line) = pass_line(slug, &report)
            {
                eprintln!("{line}");
            }
            let text = report.retry.unwrap_or_default();
            Outcome::Failed(format!("{text}, trying again"))
        }
        Ok(Ok(report)) => {
            if let Some(line) = pass_line(slug, &report) {
                eprintln!("{line}");
            }
            Outcome::Done
        }
        Ok(Err(e)) => Outcome::Failed(e.to_string()),
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

/// The accounts that exist now; `None` when the list can't be read.
async fn account_slugs(state: &Arc<AppState>) -> Option<Vec<String>> {
    let st = state.clone();
    tokio::task::spawn_blocking(move || {
        st.auth
            .accounts()
            .list()
            .map(|l| l.into_iter().map(|a| a.slug).collect())
    })
    .await
    .ok()?
    .ok()
}

/// The accounts waiting for a pass, each with the time of its next try, and the last error
/// text of those that failed.
struct Pending {
    due: BTreeMap<String, Instant>,
    errors: BTreeMap<String, String>,
    /// Counts every failed pass ([`run_counted`]).
    failed: Arc<AtomicU64>,
}

impl Pending {
    /// Run the pass of `slug` and file the outcome.
    async fn attempt(&mut self, state: &Arc<AppState>, root: &Path, slug: &str, t: Timing) {
        match pass(state, root, slug).await {
            Outcome::Done => {
                self.due.remove(slug);
                if self.errors.remove(slug).is_some() {
                    eprintln!("notedown render: {slug}: works again");
                }
            }
            Outcome::Busy => {
                self.due.insert(slug.to_string(), Instant::now() + t.poll);
            }
            Outcome::Failed(text) => {
                // An account that was removed meanwhile has nothing to render.
                if account_slugs(state)
                    .await
                    .is_some_and(|l| !l.iter().any(|s| s == slug))
                {
                    self.due.remove(slug);
                    self.errors.remove(slug);
                    return;
                }
                self.failed.fetch_add(1, Ordering::SeqCst);
                if self.errors.get(slug) != Some(&text) {
                    eprintln!("notedown render: {slug}: {text}");
                    self.errors.insert(slug.to_string(), text);
                }
                self.due.insert(slug.to_string(), Instant::now() + t.retry);
            }
        }
    }
}

async fn set_cursor(state: &Arc<AppState>, seq: u64) {
    let st = state.clone();
    match tokio::task::spawn_blocking(move || st.journal.set_cursor(CONSUMER, seq)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => eprintln!("notedown render: cursor: {e}"),
        Err(e) => eprintln!("notedown render: cursor: {e}"),
    }
}

/// What one read of the journal found.
struct Batch {
    last: u64,
    accounts: BTreeSet<String>,
    woke: bool,
    unreadable: bool,
}

fn read_batch(state: &AppState, from: u64) -> crate::Result<Batch> {
    let mut batch = Batch {
        last: from.saturating_sub(1),
        accounts: BTreeSet::new(),
        woke: false,
        unreadable: false,
    };
    for item in state.journal.read_from(from)? {
        match item {
            Ok(event) => {
                if event.seq < from {
                    continue;
                }
                batch.last = batch.last.max(event.seq);
                if wakes(&event.body) {
                    batch.woke = true;
                    batch.accounts.insert(event.account);
                }
            }
            Err(_) => batch.unreadable = true,
        }
    }
    Ok(batch)
}

/// Render at start, then after note events and a quiet time. An account whose pass failed
/// or found the lock busy is tried again (every `retry` or `poll`) and holds the cursor.
/// Never returns; stop it by aborting the task.
pub async fn run(state: Arc<AppState>, root: PathBuf, timing: Timing) {
    run_counted(state, root, timing, Arc::new(AtomicU64::new(0))).await
}

/// [`run`], counting every failed pass in `failed` (tests wait for a failure with it). Not for
/// production use.
#[doc(hidden)]
pub async fn run_counted(
    state: Arc<AppState>,
    root: PathBuf,
    timing: Timing,
    failed: Arc<AtomicU64>,
) {
    let st = state.clone();
    let start = tokio::task::spawn_blocking(move || {
        let seq = st.journal.last_seq();
        let slugs = st.auth.accounts().list();
        (seq, slugs)
    })
    .await;
    let (mut seq, slugs) = match start {
        Ok((seq, slugs)) => (
            seq.unwrap_or_else(|e| {
                eprintln!("notedown render: journal: {e}");
                0
            }),
            slugs.map(|l| l.into_iter().map(|a| a.slug).collect::<Vec<_>>()),
        ),
        Err(e) => {
            eprintln!("notedown render: {e}");
            (0, Ok(Vec::new()))
        }
    };
    let mut pending = Pending {
        due: BTreeMap::new(),
        errors: BTreeMap::new(),
        failed,
    };
    match slugs {
        Ok(slugs) => {
            for slug in slugs {
                pending.attempt(&state, &root, &slug, timing).await;
            }
        }
        Err(e) => eprintln!("notedown render: accounts: {e}"),
    }
    if pending.due.is_empty() {
        set_cursor(&state, seq).await;
    }

    let mut warned = false;
    loop {
        tokio::time::sleep(timing.poll).await;
        let st = state.clone();
        let from = seq + 1;
        match tokio::task::spawn_blocking(move || read_batch(&st, from)).await {
            Ok(Ok(batch)) => {
                if batch.unreadable && !warned {
                    warned = true;
                    eprintln!("notedown render: a journal line could not be read, going on");
                }
                if batch.last > seq {
                    seq = batch.last;
                }
                if batch.woke {
                    let at = Instant::now() + timing.quiet;
                    for slug in batch.accounts {
                        pending.due.insert(slug, at);
                    }
                } else if pending.due.is_empty() && batch.last >= from {
                    set_cursor(&state, seq).await;
                }
            }
            Ok(Err(e)) => eprintln!("notedown render: journal: {e}"),
            Err(e) => eprintln!("notedown render: journal: {e}"),
        }
        let now = Instant::now();
        let ready: Vec<String> = pending
            .due
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(slug, _)| slug.clone())
            .collect();
        if ready.is_empty() {
            continue;
        }
        for slug in ready {
            pending.attempt(&state, &root, &slug, timing).await;
        }
        if pending.due.is_empty() {
            set_cursor(&state, seq).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{EventBody, FileKind};

    #[test]
    fn note_and_folder_changes_wake_others_dont() {
        let note = |p: &str| EventBody::FileDeleted {
            id: 1,
            path: p.into(),
            recycle: "r".into(),
        };
        assert!(wakes(&note("NOTE/Note/a.note")));
        assert!(wakes(&note("NOTE/Note/a.NOTE")));
        assert!(!wakes(&note("Document/a.pdf")));
        assert!(wakes(&EventBody::FileCopied {
            id: 2,
            source_id: 1,
            path: "NOTE/b.note".into()
        }));
        assert!(wakes(&EventBody::FolderMoved {
            id: 3,
            path: "NOTE/B".into(),
            old_path: "NOTE/A".into()
        }));
        assert!(wakes(&EventBody::FolderDeleted {
            id: 3,
            path: "NOTE/B".into()
        }));
        assert!(!wakes(&EventBody::FolderCreated {
            id: 3,
            path: "NOTE/B".into()
        }));
        assert!(wakes(&EventBody::FileModified {
            id: 1,
            path: "NOTE/a.note".into(),
            kind: FileKind::Note,
            md5: "b".into(),
            prev_md5: "a".into(),
            size: 1,
            pages_changed: None,
        }));
    }
}
