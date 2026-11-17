//! What the notifications remember between two starts: `<app config dir>/notify.json`.
//!
//! Two writers share one [`NotifyStore`]: the poller (the count of the files of each device
//! sync) and the notifier (what was shown). Each changes only its own fields of a
//! [`KeyRecord`], inside [`NotifyStore::update`], which saves before the lock is let go. So
//! one writer's save never undoes the other's.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

const FILE: &str = "notify.json";
const TEMP: &str = "notify.json.tmp";
const BROKEN: &str = "notify.json.broken";

/// The record of every login, by its token key (`crate::token::token_key`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NotifyRecord {
    pub keys: BTreeMap<String, KeyRecord>,
}

/// What one login keeps. The poller writes the first four fields, the notifier the others.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct KeyRecord {
    // The poller's fields.
    /// The newest device `lastSyncMs` that was counted (the server's clock).
    pub last_sync_ms: Option<u64>,
    /// The journal seq the next count starts after.
    pub cursor: Option<u64>,
    /// The count of the last device sync.
    pub last_count: Option<SyncCount>,
    /// Since when the count fails (this Mac's clock).
    pub count_failing_since: Option<u64>,

    // The notifier's fields.
    /// The `at_ms` of the last sync count that was shown (or taken as seen).
    pub sync_shown_at: Option<u64>,
    /// When the last sync notice was shown (this Mac's clock).
    pub sync_last_notice_ms: Option<u64>,
    /// Files of syncs within the gap that were not shown yet.
    pub unshown_files: u64,
    /// The newest text seq of the notes the last OCR notice counted.
    pub ocr_text_seq: Option<u64>,
    /// The data folder `ocr_text_seq` belongs to.
    pub ocr_data_dir: Option<PathBuf>,
    /// The `lastRunMs` of the pipeline the notifier saw last.
    pub ocr_last_run_ms: Option<u64>,
    /// When the last OCR notice was shown (this Mac's clock).
    pub ocr_last_notice_ms: Option<u64>,
    /// Notes of OCR passes within the gap that were not shown yet.
    pub unshown_notes: u64,
    /// The agents, by consumer name.
    pub consumers: BTreeMap<String, ConsumerRecord>,
    /// The problems, by key: `server`, `ocr` and `oldsync`.
    pub problems: BTreeMap<String, ProblemState>,
}

/// How many files a device sync changed. `files` is `None` when it is not known.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncCount {
    /// The `lastSyncMs` of the sync (the server's clock).
    pub at_ms: u64,
    pub files: Option<u64>,
    /// When the poller stored the count (this Mac's clock), for the old-event test of the
    /// notice. `None` when the poller cannot tell the sync is recent (its first poll of the
    /// login in this run, or after a long pause such as a sleep), and in an older notify.json:
    /// the test then takes `at_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counted_at_ms: Option<u64>,
}

/// What the notifier knows of one consumer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConsumerRecord {
    /// The seq that was shown (or taken as seen).
    pub seq: u64,
    /// A seq that rose and waits for its notice.
    pub pending_seq: Option<u64>,
    /// When it rose last (this Mac's clock).
    pub rose_at: Option<u64>,
}

/// What the notifier knows of one problem.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProblemState {
    /// The problem is there now.
    pub active: bool,
    /// When its last notice was shown (this Mac's clock); it stays after the problem is gone.
    pub last_notice_ms: u64,
    /// The `lastSyncMs` an old sync problem was shown for.
    pub for_sync: Option<u64>,
}

/// notify.json behind a lock. Share it with an `Arc`.
pub struct NotifyStore {
    dir: PathBuf,
    state: Mutex<State>,
}

struct State {
    record: NotifyRecord,
    /// notify.json is there but could not be read, or a broken one could not be set aside:
    /// it is never written over. Changes stay in memory until the file reads again.
    blocked: bool,
}

/// What [`read_file`] found.
enum OnDisk {
    /// No file, or a broken one that was set aside: start empty.
    Empty,
    Record(NotifyRecord),
    /// The file is there and must not be written over.
    Blocked,
}

impl NotifyStore {
    /// Reads `<dir>/notify.json`. A missing file is an empty record. A file that does not
    /// parse is kept as `notify.json.broken` and the record starts empty: the next statuses
    /// are baselines. A file that cannot be read (or a broken one that cannot be set aside)
    /// starts empty too, but it is not written over: changes stay in memory, and each
    /// [`update`](Self::update) tries to read the file again.
    pub fn load(dir: &Path) -> Self {
        let (record, blocked) = match read_file(dir, true) {
            OnDisk::Empty => (NotifyRecord::default(), false),
            OnDisk::Record(record) => (record, false),
            OnDisk::Blocked => (NotifyRecord::default(), true),
        };
        Self {
            dir: dir.to_owned(),
            state: Mutex::new(State { record, blocked }),
        }
    }

    /// A copy of the record.
    pub fn read(&self) -> NotifyRecord {
        self.lock().record.clone()
    }

    /// Changes a copy of the record with `f` and saves it, under the lock, when `f` changed it;
    /// the copy takes the place of the record only after a good save. So a failed save is the
    /// error and leaves the record as it was, and a panic in `f` changes nothing. A login
    /// without any field set (an `entry().or_default()` alone) is no change and is not kept.
    ///
    /// While notify.json cannot be read, the file is read again first: once it reads, its
    /// record is the one `f` changes; until then the change stays in memory and nothing is
    /// saved.
    pub fn update<R>(&self, f: impl FnOnce(&mut NotifyRecord) -> R) -> io::Result<R> {
        let mut state = self.lock();
        if state.blocked {
            // Said once at the load; the tries after it are quiet.
            match read_file(&self.dir, false) {
                OnDisk::Blocked => {}
                OnDisk::Empty => state.blocked = false,
                OnDisk::Record(record) => {
                    state.record = record;
                    state.blocked = false;
                }
            }
        }
        let mut next = state.record.clone();
        let result = f(&mut next);
        next.keys.retain(|_, key| *key != KeyRecord::default());
        if next != state.record {
            if !state.blocked {
                write(&self.dir, &next)?;
            }
            state.record = next;
        }
        Ok(result)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A writer that panicked changed nothing: `update` changes a copy.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Reads `<dir>/notify.json`; sets a broken one aside. `loud` logs a file that is kept.
fn read_file(dir: &Path, loud: bool) -> OnDisk {
    let path = dir.join(FILE);
    match fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(record) => OnDisk::Record(record),
            Err(e) => match fs::rename(&path, dir.join(BROKEN)) {
                Ok(()) => {
                    eprintln!(
                        "notedown: {} does not parse ({e}); kept as {BROKEN}",
                        path.display()
                    );
                    OnDisk::Empty
                }
                Err(why) => {
                    if loud {
                        eprintln!(
                            "notedown: {} does not parse ({e}) and could not be set aside \
                             ({why}); it is not written over",
                            path.display()
                        );
                    }
                    OnDisk::Blocked
                }
            },
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => OnDisk::Empty,
        Err(e) => {
            if loud {
                eprintln!(
                    "notedown: {} could not be read ({e}); it is not written over",
                    path.display()
                );
            }
            OnDisk::Blocked
        }
    }
}

/// Writes the record durably: a temp file flushed to disk, then renamed over the old one.
fn write(dir: &Path, record: &NotifyRecord) -> io::Result<()> {
    use std::io::Write;
    fs::create_dir_all(dir)?;
    let temp = dir.join(TEMP);
    let json = serde_json::to_vec_pretty(record).map_err(io::Error::other)?;
    let written = (|| {
        let mut file = fs::File::create(&temp)?;
        file.write_all(&json)?;
        file.sync_all()?;
        fs::rename(&temp, dir.join(FILE))
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
        return written;
    }
    // The rename is flushed too; the new file is in place already, so a failure is only logged.
    if let Err(e) = fs::File::open(dir).and_then(|folder| folder.sync_all()) {
        eprintln!("notedown: the folder of {FILE} could not be flushed: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
