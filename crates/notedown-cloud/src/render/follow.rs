//! The `render` consumer: one pass per account at start, then a pass for each account whose
//! notes changed, after a quiet time, so a sync of many notes is one pass.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
}

impl Timing {
    pub const DEFAULT: Timing = Timing {
        quiet: Duration::from_secs(2),
        poll: Duration::from_secs(1),
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

/// One pass for `slug`, off the async threads. `true` when the account was busy.
async fn pass(state: &Arc<AppState>, root: &Path, slug: &str) -> bool {
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
        Ok(Ok(report)) => {
            if let Some(line) = pass_line(slug, &report) {
                eprintln!("{line}");
            }
            report.busy
        }
        Ok(Err(e)) => {
            eprintln!("notedown render: {slug}: {e}");
            false
        }
        Err(e) => {
            eprintln!("notedown render: {slug}: {e}");
            false
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

/// Render at start, then after note events and a quiet time. Never returns; stop it by
/// aborting the task.
pub async fn run(state: Arc<AppState>, root: PathBuf, timing: Timing) {
    let st = state.clone();
    let mut seq = match tokio::task::spawn_blocking(move || st.journal.last_seq()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            eprintln!("notedown render: journal: {e}");
            0
        }
        Err(e) => {
            eprintln!("notedown render: journal: {e}");
            0
        }
    };
    match state.auth.accounts().list() {
        Ok(accounts) => {
            for a in accounts {
                pass(&state, &root, &a.slug).await;
            }
        }
        Err(e) => eprintln!("notedown render: accounts: {e}"),
    }
    set_cursor(&state, seq).await;

    let mut pending: BTreeSet<String> = BTreeSet::new();
    let mut last_wake = Instant::now();
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
                    pending.extend(batch.accounts);
                    last_wake = Instant::now();
                } else if pending.is_empty() && batch.last >= from {
                    set_cursor(&state, seq).await;
                }
            }
            Ok(Err(e)) => eprintln!("notedown render: journal: {e}"),
            Err(e) => eprintln!("notedown render: journal: {e}"),
        }
        if pending.is_empty() || last_wake.elapsed() < timing.quiet {
            continue;
        }
        let mut busy = BTreeSet::new();
        for slug in std::mem::take(&mut pending) {
            if pass(&state, &root, &slug).await {
                busy.insert(slug);
            }
        }
        pending = busy;
        if pending.is_empty() {
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
