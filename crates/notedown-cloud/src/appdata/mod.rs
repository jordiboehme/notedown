//! App data of one account: the digests ("summary", `docs/sync-protocol.md` 5) and the to-dos
//! (6.1) of the device, stored as records under `accounts/<slug>/`:
//!
//! - `digests/items/<id>.json` (one digest each), `digests/groups/<id>.json`,
//!   `digests/tags.json`, `digests/marks/<name>` (the handwritten comments, opaque `.mark`
//!   bytes under the device's name).
//! - `tasks/groups/<taskListId>.json`, `tasks/items/<taskId>.json`,
//!   `tasks/sort/<taskListId>.json`.
//!
//! Records are JSON with the wire names of the protocol in its order, two-space indent and a
//! trailing newline, written atomically. Fields the device sends that this module does not name
//! are kept in `extra` and sent back unchanged. Server-only fields (`created_at_ms`,
//! `updated_at_ms`, `deleted_at_ms`) are stored too; `wire()` of a record drops them.
//!
//! Every read and write takes the account's store lock ([`WriteGuard`] from
//! [`Store::lock_recovered`]), so app data writes serialize with file writes and the journal
//! order holds. A write stores the record first and then appends its journal event; a crash in
//! between loses that one event, the record is right (accepted: the next change or a
//! `journal snapshot` catches up).
//!
//! Ids: digests and digest groups share one id space (the server ids the device knows); a new
//! record gets the largest id of both plus one ([`AppData::next_digest_id`]). Tags have their own
//! space. To-do ids come from the device and are kept as sent; they must be plain file names
//! ([`valid_task_key`]).
//!
//! Deletes: digests and groups are soft-deleted (`isDeleted: "Y"`, the file stays). A deleted
//! task stays as a tombstone (`isDeleted: "Y"`, `deleted_at_ms`) for the incremental pull and is
//! swept after [`TOMBSTONE_TTL_MS`] ([`AppData::sweep_tombstones`]). To-do groups and sorts are
//! removed.

mod digests;
mod tasks;

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

pub use digests::{Digest, DigestGroup, DigestTag};
pub use tasks::{Task, TaskGroup, TaskSort};

use crate::journal::{Event, EventBody, Journal, Origin, new_op_id};
use crate::store::{Store, WriteGuard};
use crate::{Error, Result, io_err};

/// `accounts/<slug>/digests`.
pub const DIGESTS_DIR: &str = "digests";
/// `accounts/<slug>/tasks`.
pub const TASKS_DIR: &str = "tasks";
/// A task tombstone older than this (5 days, ms) is swept; the device's sync tokens live as long
/// (`docs/sync-protocol.md` 6.1).
pub const TOMBSTONE_TTL_MS: u64 = 5 * 24 * 60 * 60 * 1000;
/// The id of the first digest or group of an account without any.
pub const FIRST_DIGEST_ID: u64 = 1;
/// The id of the first tag.
pub const FIRST_TAG_ID: u64 = 1;
/// The longest accepted `.mark` name, in bytes.
pub const MAX_MARK_NAME: usize = 64;
/// The longest accepted `taskId` or `taskListId`, in bytes.
pub const MAX_TASK_KEY: usize = 128;

/// `"Y"`: the value of `isDeleted` (and `isSummaryGroup`) for yes.
pub const YES: &str = "Y";
/// `"N"`: the value of `isDeleted` (and `isSummaryGroup`) for no.
pub const NO: &str = "N";

/// Whether `name` may be stored as a handwritten comment: it ends in `.mark`, has at most
/// [`MAX_MARK_NAME`] bytes, no `/`, backslash or NUL and no leading dot. The device's names are
/// 32 hex characters plus `.mark`; case is kept.
pub fn valid_mark_name(name: &str) -> bool {
    name.len() > ".mark".len()
        && name.len() <= MAX_MARK_NAME
        && name.ends_with(".mark")
        && !name.starts_with('.')
        && !name.contains(['/', '\\', '\0'])
}

/// Whether `key` may be used as a `taskId` or `taskListId` (a file name): 1 to
/// [`MAX_TASK_KEY`] ASCII letters, digits, `-` and `_`. The device makes 32 hex characters.
pub fn valid_task_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_TASK_KEY
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The kind of a record change, from whether the record was live before and is live after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Created,
    Updated,
    Deleted,
}

/// Created when the record was absent or deleted and is live now, updated when it stays live,
/// deleted when a live record is deleted. A deleted record that stays deleted is no change.
fn change(was_live: bool, is_live: bool) -> Option<Change> {
    match (was_live, is_live) {
        (false, true) => Some(Change::Created),
        (true, true) => Some(Change::Updated),
        (true, false) => Some(Change::Deleted),
        (false, false) => None,
    }
}

/// The app data of one account. Cheap; make one per request.
#[derive(Debug, Clone, Copy)]
pub struct AppData<'a> {
    store: &'a Store,
}

impl<'a> AppData<'a> {
    /// The app data of the account of `store`.
    pub fn new(store: &'a Store) -> AppData<'a> {
        AppData { store }
    }

    fn check(&self, guard: &WriteGuard) -> Result<()> {
        self.store.check_guard(guard)
    }

    fn digests_dir(&self) -> PathBuf {
        self.store.account_dir().join(DIGESTS_DIR)
    }

    fn tasks_dir(&self) -> PathBuf {
        self.store.account_dir().join(TASKS_DIR)
    }

    /// Append `bodies` as one batch with one `opId`. Nothing for an empty list.
    fn journal(
        &self,
        bodies: Vec<EventBody>,
        origin: Origin,
        device: Option<&str>,
        journal: &Journal,
    ) -> Result<()> {
        if bodies.is_empty() {
            return Ok(());
        }
        let op_id = new_op_id();
        let mut events: Vec<Event> = bodies
            .into_iter()
            .map(|body| {
                Event::new(
                    self.store.slug(),
                    origin,
                    device.map(String::from),
                    &op_id,
                    body,
                )
            })
            .collect();
        journal.append(&mut events)?;
        Ok(())
    }

    /// The events of `journal snapshot` for the app data: `digest.created` for every live
    /// digest, then `task.created` for every live task. The caller holds the store lock.
    pub(crate) fn snapshot_bodies(&self, guard: &WriteGuard) -> Result<Vec<EventBody>> {
        let mut out: Vec<EventBody> = self
            .digests(guard)?
            .iter()
            .filter(|d| d.is_live())
            .map(|d| EventBody::DigestCreated(d.info()))
            .collect();
        out.extend(
            self.tasks(guard)?
                .iter()
                .filter(|t| t.is_live())
                .map(|t| EventBody::TaskCreated(t.info())),
        );
        Ok(out)
    }
}

fn corrupt(path: &Path, message: impl std::fmt::Display) -> Error {
    Error::Config {
        path: path.to_path_buf(),
        message: message.to_string(),
    }
}

/// The record in `path`, `None` when the file does not exist.
fn read_record<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| corrupt(path, e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(path)(e)),
    }
}

/// Write a record atomically (two-space indent, trailing newline); creates the folder.
fn write_record<T: Serialize>(path: &Path, record: &T) -> Result<()> {
    crate::fsutil::write_atomic(path, &crate::fsutil::json_bytes(record), false)
}

/// Remove a file and fsync its folder; false when it did not exist.
fn remove_record(path: &Path) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => {
            if let Some(dir) = path.parent() {
                crate::fsutil::fsync_dir(dir)?;
            }
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io_err(path)(e)),
    }
}

/// The stems of the `<stem>.json` files in `dir`, sorted; none when `dir` does not exist. Dot
/// files (temp files of atomic writes) are skipped.
fn record_stems(dir: &Path) -> Result<Vec<String>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(dir)(e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_err(dir))?;
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
            continue;
        };
        if stem.is_empty() || stem.starts_with('.') {
            continue;
        }
        if entry.file_type().map_err(io_err(dir))?.is_file() {
            out.push(stem.to_string());
        }
    }
    out.sort();
    Ok(out)
}

/// The numeric ids of the `<id>.json` files in `dir`, ascending.
fn record_ids(dir: &Path) -> Result<Vec<u64>> {
    let mut ids: Vec<u64> = record_stems(dir)?
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();
    ids.sort_unstable();
    Ok(ids)
}

/// Every record in `dir` (`<stem>.json`), in stem order.
fn read_records<T: DeserializeOwned>(dir: &Path) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for stem in record_stems(dir)? {
        if let Some(r) = read_record(&dir.join(format!("{stem}.json")))? {
            out.push(r);
        }
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    use time::OffsetDateTime;

    use crate::Clock;
    use crate::auth::Accounts;
    use crate::journal::{Journal, JournalOptions};
    use crate::store::Store;

    /// 2026-10-06T12:00:00Z
    pub const T0: i64 = 1_791_288_000_000;

    pub struct TestClock(pub AtomicI64);

    impl Clock for TestClock {
        fn now(&self) -> OffsetDateTime {
            OffsetDateTime::from_unix_timestamp_nanos(
                self.0.load(Ordering::SeqCst) as i128 * 1_000_000,
            )
            .unwrap()
        }
    }

    /// An account `jordi` with its store and a journal on a settable clock.
    pub struct Fixture {
        pub _dir: tempfile::TempDir,
        pub store: Store,
        pub journal: Journal,
        pub clock: Arc<TestClock>,
    }

    impl Fixture {
        pub fn new() -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            Accounts::new(dir.path())
                .create("jordi", "jordi@example.org", "secret")
                .unwrap();
            let store = Store::open(dir.path(), "jordi").unwrap();
            let clock = Arc::new(TestClock(AtomicI64::new(T0)));
            let journal = Journal::open_with(
                dir.path(),
                JournalOptions {
                    max_file_bytes: 1 << 20,
                    clock: clock.clone(),
                },
            )
            .unwrap();
            Fixture {
                _dir: dir,
                store,
                journal,
                clock,
            }
        }

        pub fn set_now(&self, ms: i64) {
            self.clock.0.store(ms, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_names() {
        assert!(valid_mark_name("0123456789abcdef0123456789ABCDEF.mark"));
        for bad in [
            ".mark",
            ".x.mark",
            "x.txt",
            "a/b.mark",
            "a\\b.mark",
            "a\0.mark",
            &format!("{}.mark", "a".repeat(60)),
        ] {
            assert!(!valid_mark_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn task_keys() {
        assert!(valid_task_key("0123456789abcdef0123456789abcdef"));
        assert!(valid_task_key("A-b_9"));
        for bad in ["", "..", "a/b", "a.json", ".x", &"a".repeat(129)] {
            assert!(!valid_task_key(bad), "{bad:?}");
        }
    }

    #[test]
    fn change_kinds() {
        assert_eq!(change(false, true), Some(Change::Created));
        assert_eq!(change(true, true), Some(Change::Updated));
        assert_eq!(change(true, false), Some(Change::Deleted));
        assert_eq!(change(false, false), None);
    }
}
