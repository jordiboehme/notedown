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
//! Reads: the lists (`digests`, `tasks`, ...) and the single reads skip a record file that cannot
//! be read or parsed, with a warning on stderr, so one bad file never stops the device's sync;
//! a `put_*` over it replaces it (event `created`). The `*_strict` lists fail instead, for
//! `journal snapshot` and the CLI. `tags.json` is always read strictly.
//!
//! Deletes: digests and groups are soft-deleted (`isDeleted: "Y"`, the file stays). A deleted
//! task stays as a tombstone (`isDeleted: "Y"`, `deleted_at_ms`) for the incremental pull and is
//! swept after [`TOMBSTONE_TTL_MS`] ([`AppData::sweep_tombstones`]). To-do groups and sorts are
//! removed.

mod digests;
pub mod import;
mod tasks;

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

pub use digests::{Digest, DigestGroup, DigestTag};
pub use import::{AppImportOptions, AppImportReport, dry_run_app_data, import_app_data};
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

/// Read `null` as the type's default: serde's `default` covers only a missing field, and the
/// device may send an explicit `null`.
fn null_default<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// Read `null` as [`NO`] (for `isDeleted` and `isSummaryGroup`).
fn null_as_no<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_else(|| NO.to_string()))
}

/// Read a `taskId` or `taskListId`: a JSON string as it is, an integer as its decimal string,
/// `null` as empty. Anything else is an error.
fn string_or_int<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    use serde::de::Error as _;
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Null => Ok(String::new()),
        serde_json::Value::Number(n) if n.is_u64() || n.is_i64() => Ok(n.to_string()),
        other => Err(D::Error::custom(format!(
            "expected a string or an integer, got {other}"
        ))),
    }
}

/// How reads treat a record file that cannot be read or parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Skip it with a warning on stderr (the device routes: one bad file must not stop the
    /// sync, and the device's next write replaces it).
    Lenient,
    /// Fail (`journal snapshot` and the CLI stay loud).
    Strict,
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

    /// The record in `path`, `None` when it is missing. With [`Mode::Lenient`] a record that
    /// cannot be read or parsed is `None` too, with a warning on stderr.
    fn load<T: DeserializeOwned>(&self, path: &Path, mode: Mode) -> Result<Option<T>> {
        match (read_record(path), mode) {
            (Err(e), Mode::Lenient) => {
                eprintln!(
                    "warning: account {}: skipping an unreadable record: {e}",
                    self.store.slug()
                );
                Ok(None)
            }
            (r, _) => r,
        }
    }

    /// Every record in `dir` (`<stem>.json`), in stem order; see [`AppData::load`] for `mode`.
    fn load_all<T: DeserializeOwned>(&self, dir: &Path, mode: Mode) -> Result<Vec<T>> {
        let mut out = Vec::new();
        for stem in record_stems(dir)? {
            if let Some(r) = self.load(&dir.join(format!("{stem}.json")), mode)? {
                out.push(r);
            }
        }
        Ok(out)
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
            .digests_strict(guard)?
            .iter()
            .filter(|d| d.is_live())
            .map(|d| EventBody::DigestCreated(d.info()))
            .collect();
        out.extend(
            self.tasks_strict(guard)?
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
    use super::testutil::Fixture;
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
    fn an_explicit_null_reads_as_the_default() {
        let d: Digest = serde_json::from_str(
            r#"{"id":null,"userId":null,"isSummaryGroup":null,"isDeleted":null,"createTime":null,"updateTime":null,"content":null}"#,
        )
        .unwrap();
        assert_eq!(
            (d.id, d.user_id, d.create_time, d.update_time),
            (0, 0, 0, 0)
        );
        assert_eq!(
            (d.is_summary_group.as_str(), d.is_deleted.as_str()),
            (NO, NO)
        );
        let g: DigestGroup = serde_json::from_str(r#"{"id":null,"isDeleted":null}"#).unwrap();
        assert_eq!((g.id, g.is_deleted.as_str()), (0, NO));
        let t: Task =
            serde_json::from_str(r#"{"taskId":"t","taskListId":null,"isDeleted":null}"#).unwrap();
        assert_eq!((t.task_list_id.as_str(), t.is_deleted.as_str()), ("", NO));
        let l: TaskGroup =
            serde_json::from_str(r#"{"taskListId":null,"userId":null,"isDeleted":null}"#).unwrap();
        assert_eq!(
            (l.task_list_id.as_str(), l.user_id, l.is_deleted.as_str()),
            ("", 0, NO)
        );
        let s: TaskSort = serde_json::from_str(r#"{"taskListId":null}"#).unwrap();
        assert_eq!(s.task_list_id, "");
    }

    #[test]
    fn task_keys_may_be_json_integers() {
        let t: Task =
            serde_json::from_str(r#"{"taskId":123,"taskListId":895334188016730112}"#).unwrap();
        assert_eq!(
            (t.task_id.as_str(), t.task_list_id.as_str()),
            ("123", "895334188016730112")
        );
        let l: TaskGroup = serde_json::from_str(r#"{"taskListId":-4}"#).unwrap();
        assert_eq!(l.task_list_id, "-4");
        let s: TaskSort = serde_json::from_str(r#"{"taskListId":42}"#).unwrap();
        assert_eq!(s.task_list_id, "42");
        assert!(serde_json::from_str::<Task>(r#"{"taskId":1.5}"#).is_err());
        assert!(serde_json::from_str::<Task>(r#"{"taskId":[1]}"#).is_err());
    }

    #[test]
    fn lenient_reads_skip_a_garbage_record_and_a_put_replaces_it() {
        let f = Fixture::new();
        let app = AppData::new(&f.store);
        let g = f.store.lock_recovered(&f.journal).unwrap();
        let live = Digest {
            id: 1,
            ..Digest::default()
        };
        app.put_digest(&g, &live, Origin::Cli, None, &f.journal)
            .unwrap();
        let items = f.store.account_dir().join("digests/items");
        std::fs::write(items.join("2.json"), b"garbage").unwrap();
        let ids: Vec<u64> = app.digests(&g).unwrap().iter().map(|d| d.id).collect();
        assert_eq!(ids, vec![1]);
        assert!(app.digests_strict(&g).is_err());
        assert_eq!(app.digest(&g, 2).unwrap(), None);
        // The 5.7 heal: the device sends the digest again.
        let healed = Digest {
            id: 2,
            content: Some("back".into()),
            ..Digest::default()
        };
        app.put_digest(&g, &healed, Origin::Device, Some("SN1"), &f.journal)
            .unwrap();
        assert_eq!(app.digests_strict(&g).unwrap().len(), 2);
        let last: Vec<_> = f
            .journal
            .read_from(0)
            .unwrap()
            .map(|e| e.unwrap())
            .collect();
        assert_eq!(last.last().unwrap().body.type_name(), "digest.created");

        let groups = f.store.account_dir().join("digests/groups");
        std::fs::create_dir_all(&groups).unwrap();
        std::fs::write(groups.join("3.json"), b"{").unwrap();
        assert!(app.digest_groups(&g).unwrap().is_empty());
        assert!(app.digest_groups_strict(&g).is_err());
        let group = DigestGroup {
            id: 3,
            ..DigestGroup::default()
        };
        app.put_digest_group(&g, &group, Origin::Cli, None, &f.journal)
            .unwrap();
        assert_eq!(app.digest_groups_strict(&g).unwrap().len(), 1);

        let tdir = f.store.account_dir().join("tasks/items");
        std::fs::create_dir_all(&tdir).unwrap();
        std::fs::write(tdir.join("t1.json"), b"[").unwrap();
        assert!(app.tasks(&g).unwrap().is_empty());
        assert!(app.tasks_strict(&g).is_err());
        assert_eq!(app.task(&g, "t1").unwrap(), None);
        let t = Task {
            task_id: "t1".into(),
            ..Task::default()
        };
        app.put_task(&g, &t, Origin::Cli, None, &f.journal).unwrap();
        assert_eq!(app.tasks_strict(&g).unwrap().len(), 1);
        let last: Vec<_> = f
            .journal
            .read_from(0)
            .unwrap()
            .map(|e| e.unwrap())
            .collect();
        assert_eq!(last.last().unwrap().body.type_name(), "task.created");

        for (dir, strict) in [("tasks/groups", true), ("tasks/sort", false)] {
            let d = f.store.account_dir().join(dir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("l1.json"), b"x").unwrap();
            if strict {
                assert!(app.task_groups(&g).unwrap().is_empty());
                assert!(app.task_groups_strict(&g).is_err());
            } else {
                assert!(app.task_sorts(&g).unwrap().is_empty());
                assert!(app.task_sorts_strict(&g).is_err());
            }
        }
    }

    #[test]
    fn a_failed_batch_journals_the_tasks_written_before_it() {
        let f = Fixture::new();
        let app = AppData::new(&f.store);
        let g = f.store.lock_recovered(&f.journal).unwrap();
        // A folder where t2's record goes: its write fails after t1's succeeded.
        std::fs::create_dir_all(f.store.account_dir().join("tasks/items/t2.json/x")).unwrap();
        let task = |id: &str| Task {
            task_id: id.into(),
            title: Some(id.into()),
            ..Task::default()
        };
        let err = app.put_tasks(
            &g,
            &[task("t1"), task("t2"), task("t3")],
            Origin::Cli,
            None,
            &f.journal,
        );
        assert!(err.is_err());
        let ev: Vec<_> = f
            .journal
            .read_from(0)
            .unwrap()
            .map(|e| e.unwrap())
            .collect();
        assert_eq!(ev.len(), 1);
        assert!(
            ev[0]
                .to_line()
                .contains(r#""type":"task.created","taskId":"t1""#)
        );
        assert!(f.store.account_dir().join("tasks/items/t1.json").is_file());
        assert!(!f.store.account_dir().join("tasks/items/t3.json").exists());
    }

    #[test]
    fn id_0_is_refused() {
        let f = Fixture::new();
        let app = AppData::new(&f.store);
        let g = f.store.lock_recovered(&f.journal).unwrap();
        let d = Digest::default();
        assert!(matches!(
            app.put_digest(&g, &d, Origin::Cli, None, &f.journal),
            Err(Error::Invalid { .. })
        ));
        let grp = DigestGroup::default();
        assert!(matches!(
            app.put_digest_group(&g, &grp, Origin::Cli, None, &f.journal),
            Err(Error::Invalid { .. })
        ));
        assert!(app.digests_strict(&g).unwrap().is_empty());
        assert_eq!(f.journal.last_seq().unwrap(), 0);
    }

    #[test]
    fn change_kinds() {
        assert_eq!(change(false, true), Some(Change::Created));
        assert_eq!(change(true, true), Some(Change::Updated));
        assert_eq!(change(true, false), Some(Change::Deleted));
        assert_eq!(change(false, false), None);
    }
}
