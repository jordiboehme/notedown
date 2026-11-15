//! The pipeline on a second machine: it reads the server's folder through `ServerData` and
//! keeps its own data (note text, cursors) in a local folder.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use notedown_cloud::auth::Accounts;
use notedown_cloud::journal::Journal;
use notedown_pipeline::Result;
use notedown_pipeline::cursor;
use notedown_pipeline::model::{LineChange, LogLine};
use notedown_pipeline::records::{Record, Records};
use notedown_pipeline::setup::Setup;
use notedown_pipeline::source::{FolderSource, JournalBatch, NoteBytes, ServerAccount, ServerData};
use notedown_pipeline::store::TextStore;
use notedown_pipeline::worker::{Clock, Worker};
use tempfile::TempDir;

use crate::source::Flaky;
use crate::worker::{
    Data, SLUG, TestClock, engine_reading, no_settle, now_ms, setup, stop_at, tree,
};

/// The server from `setup()` and the Mac's local folder, whose `pipeline.toml` points at it.
fn two_folders() -> (Data, TempDir) {
    let d = setup();
    let local = tempfile::tempdir().unwrap();
    point_at(local.path(), &d.data);
    (d, local)
}

fn point_at(local: &Path, server: &Path) {
    std::fs::write(
        local.join("pipeline.toml"),
        format!("[server]\nfolder = {:?}\n", server.to_str().unwrap()),
    )
    .unwrap();
}

fn load(local: &Path) -> Setup {
    let mut s = Setup::load(local).unwrap();
    s.cfg = no_settle();
    s
}

/// What the worker said, line by line.
#[derive(Clone, Default)]
struct Said(Arc<Mutex<Vec<String>>>);

impl Said {
    fn sink(&self) -> Arc<dyn Fn(&str) + Send + Sync> {
        let lines = self.0.clone();
        Arc::new(move |s: &str| lines.lock().unwrap().push(s.to_string()))
    }
    fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
    fn count(&self, part: &str) -> usize {
        self.lines().iter().filter(|l| l.contains(part)).count()
    }
}

fn log(local: &Path) -> Vec<LogLine> {
    let mut out = Vec::new();
    let Ok(store) = TextStore::open(local, SLUG) else {
        return out;
    };
    store
        .read_log(0, None, &mut |l| {
            out.push(l);
            Ok(())
        })
        .unwrap();
    out
}

fn last_seq(data: &Path) -> u64 {
    Journal::open(data).unwrap().last_seq().unwrap()
}

#[test]
fn the_worker_reads_one_folder_and_writes_another() {
    let (d, local) = two_folders();
    let key = d.put("test1", "NOTE/Note");
    let before = tree(&d.data);
    let s = load(local.path());
    assert!(s.two_machines);
    Worker::new(&s, Box::new(engine_reading("hello")))
        .run_once(true)
        .unwrap();
    let store = TextStore::open(local.path(), SLUG).unwrap();
    assert!(store.note(&key).unwrap().is_some(), "the note is local");
    assert!(local.path().join("pipeline").join(SLUG).is_dir());
    assert_eq!(tree(&d.data), before, "the server folder is untouched");
    assert!(!d.data.join("pipeline").exists());
    assert!(!d.data.join("journal/consumers/pipeline.cursor").exists());
}

/// Makes a folder read-only for the test and gives the write modes back when dropped.
struct ReadOnly {
    modes: Vec<(PathBuf, u32)>,
}

impl ReadOnly {
    fn set(dir: &Path) -> ReadOnly {
        use std::os::unix::fs::PermissionsExt;
        let mut modes = Vec::new();
        let mut all = vec![(dir.to_path_buf(), 0, std::time::SystemTime::UNIX_EPOCH)];
        all.extend(tree(dir));
        for (p, _, _) in all {
            let mode = std::fs::metadata(&p).unwrap().permissions().mode();
            modes.push((p, mode));
        }
        for (p, mode) in &modes {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode & !0o222)).unwrap();
        }
        ReadOnly { modes }
    }
}

impl Drop for ReadOnly {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        for (p, mode) in self.modes.iter().rev() {
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(*mode));
        }
    }
}

#[test]
fn a_read_only_server_folder_is_never_written() {
    let (d, local) = two_folders();
    let key = d.put("test1", "NOTE/Note");
    let _guard = ReadOnly::set(&d.data);
    let probe = d.data.join("probe");
    if std::fs::write(&probe, b"x").is_ok() {
        let _ = std::fs::remove_file(&probe);
        eprintln!("skipped: running as root, a read-only folder can be written");
        return;
    }
    let s = load(local.path());
    Worker::new(&s, Box::new(engine_reading("hello")))
        .run_once(true)
        .unwrap();
    let store = TextStore::open(local.path(), SLUG).unwrap();
    let state = store.note(&key).unwrap().unwrap();
    assert_eq!(state.pages[0].blocks[0].text, "hello");
}

#[test]
fn the_cursor_lives_in_the_local_folder() {
    let (d, local) = two_folders();
    d.put("test1", "NOTE/Note");
    let clock = TestClock::at(now_ms());
    let mut w = Worker::new(&load(local.path()), Box::new(engine_reading("hello")))
        .with_clock(Arc::new(clock.clone()));
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    assert_eq!(log(local.path()).len(), 1);
    assert_eq!(cursor::read(local.path(), SLUG).unwrap(), last_seq(&d.data));
    assert!(
        local
            .path()
            .join("pipeline")
            .join(format!("{SLUG}.cursor"))
            .is_file()
    );
    assert!(!d.data.join("journal/consumers/pipeline.cursor").exists());
}

#[test]
fn a_failing_source_is_retried_and_deletes_nothing() {
    let (d, local) = two_folders();
    let key1 = d.put("test1", "NOTE/Note");
    let fail = Arc::new(AtomicBool::new(false));
    let mut s = load(local.path());
    s.source = Arc::new(Flaky {
        inner: FolderSource::new(&d.data),
        fail: fail.clone(),
    });
    let said = Said::default();
    let clock = TestClock::at(now_ms());
    let mut w = Worker::new(&s, Box::new(engine_reading("hello")))
        .with_clock(Arc::new(clock.clone()))
        .with_messages(said.sink());
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    assert_eq!(log(local.path()).len(), 1);
    let first = cursor::read(local.path(), SLUG).unwrap();
    assert_eq!(first, last_seq(&d.data));

    let key2 = d.put("test2", "NOTE/Note");
    fail.store(true, Ordering::SeqCst);
    let state = TextStore::open(local.path(), SLUG)
        .unwrap()
        .note(&key1)
        .unwrap();
    // Many polls and more than one rescan time.
    w.run(stop_at(&clock, clock.now_ms() + 25 * 60_000))
        .unwrap();
    let lines = log(local.path());
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines.iter().all(|l| l.change != LineChange::NoteDeleted));
    let store = TextStore::open(local.path(), SLUG).unwrap();
    assert_eq!(store.note(&key1).unwrap(), state, "the store is unchanged");
    assert!(store.note(&key2).unwrap().is_none());
    assert_eq!(cursor::read(local.path(), SLUG).unwrap(), first);
    assert_eq!(said.count("the server is away"), 1, "{:?}", said.lines());

    fail.store(false, Ordering::SeqCst);
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    let store = TextStore::open(local.path(), SLUG).unwrap();
    assert!(store.note(&key2).unwrap().is_some(), "test2 is read");
    assert_eq!(log(local.path()).len(), 2);
    assert_eq!(cursor::read(local.path(), SLUG).unwrap(), last_seq(&d.data));
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let target = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &target);
        } else {
            std::fs::copy(e.path(), &target).unwrap();
        }
    }
}

#[test]
fn a_server_folder_that_appears_later_is_read() {
    let prepared = setup();
    let key = prepared.put("test1", "NOTE/Note");
    let tmp = tempfile::tempdir().unwrap();
    let later = tmp.path().join("later");
    let local = tempfile::tempdir().unwrap();
    point_at(local.path(), &later);
    let clock = TestClock::at(now_ms());
    let start = clock.now_ms();
    let mut w = Worker::new(&load(local.path()), Box::new(engine_reading("hello")))
        .with_clock(Arc::new(clock.clone()))
        .with_messages(Said::default().sink());
    let copied = AtomicBool::new(false);
    let c = clock.clone();
    let from = prepared.data.clone();
    let to = later.clone();
    w.run(move || {
        let now = c.now_ms();
        if now >= start + 5_000 && !copied.swap(true, Ordering::SeqCst) {
            copy_dir(&from, &to);
        }
        now >= start + 10_000
    })
    .unwrap();
    let store = TextStore::open(local.path(), SLUG).unwrap();
    assert!(store.note(&key).unwrap().is_some(), "the note is read");
    assert_eq!(log(local.path()).len(), 1);
}

#[test]
fn a_cursor_ahead_of_the_journal_starts_over() {
    let (d, local) = two_folders();
    d.put("test1", "NOTE/Note");
    let end = last_seq(&d.data);
    cursor::write(local.path(), SLUG, 10_000).unwrap();
    let said = Said::default();
    let clock = TestClock::at(now_ms());
    let mut w = Worker::new(&load(local.path()), Box::new(engine_reading("hello")))
        .with_clock(Arc::new(clock.clone()))
        .with_messages(said.sink());
    w.run(stop_at(&clock, clock.now_ms() + 5_000)).unwrap();
    let message = format!(
        "notedown pipeline: the journal of {SLUG} ends at {end}, before this pipeline's \
         position 10000; reading it again from the start"
    );
    assert_eq!(
        said.lines().iter().filter(|l| **l == message).count(),
        1,
        "{:?}",
        said.lines()
    );
    assert_eq!(cursor::read(local.path(), SLUG).unwrap(), end);
}

/// A folder source that notes the `from` of every journal read.
struct Recording {
    inner: FolderSource,
    froms: Arc<Mutex<Vec<u64>>>,
}

impl ServerData for Recording {
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn accounts(&self) -> Result<Vec<ServerAccount>> {
        self.inner.accounts()
    }
    fn records(&self, slug: &str) -> Result<Records> {
        self.inner.records(slug)
    }
    fn record(&self, slug: &str, key: &str) -> Result<Option<Record>> {
        self.inner.record(slug, key)
    }
    fn note(&self, slug: &str, path: &str) -> Result<NoteBytes> {
        self.inner.note(slug, path)
    }
    fn note_exists(&self, slug: &str, path: &str) -> Result<Option<bool>> {
        self.inner.note_exists(slug, path)
    }
    fn digests(&self, slug: &str) -> Result<Vec<notedown_cloud::appdata::Digest>> {
        self.inner.digests(slug)
    }
    fn journal(&self, slug: &str, from: u64, limit: usize) -> Result<JournalBatch> {
        self.froms.lock().unwrap().push(from);
        self.inner.journal(slug, from, limit)
    }
}

#[test]
fn one_machine_takes_over_the_old_cursor() {
    let d = setup();
    d.put("test1", "NOTE/Note");
    let journal = Journal::open(&d.data).unwrap();
    let n = journal.last_seq().unwrap();
    assert!(n > 1);
    journal.set_cursor("pipeline", n - 1).unwrap();
    let froms = Arc::new(Mutex::new(Vec::new()));
    let mut s = load(&d.data);
    assert!(!s.two_machines);
    s.source = Arc::new(Recording {
        inner: FolderSource::new(&d.data),
        froms: froms.clone(),
    });
    let clock = TestClock::at(now_ms());
    let mut w =
        Worker::new(&s, Box::new(engine_reading("hello"))).with_clock(Arc::new(clock.clone()));
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    assert_eq!(
        froms.lock().unwrap().first(),
        Some(&n),
        "starts after the old cursor"
    );
    assert_eq!(cursor::read(&d.data, SLUG).unwrap(), n);
    // The old file stays as it was.
    assert_eq!(journal.cursor("pipeline").unwrap().unwrap().seq, n - 1);
}

/// A folder source whose `records` or `note` fail while the flag is set; the rest works, so
/// the account is listed and its journal is read.
struct Partial {
    inner: FolderSource,
    records: Arc<AtomicBool>,
    note: Arc<AtomicBool>,
}

fn away(flag: &AtomicBool) -> Result<()> {
    if flag.load(Ordering::SeqCst) {
        Err(notedown_pipeline::Error::Read("the share is away".into()))
    } else {
        Ok(())
    }
}

impl ServerData for Partial {
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn accounts(&self) -> Result<Vec<ServerAccount>> {
        self.inner.accounts()
    }
    fn records(&self, slug: &str) -> Result<Records> {
        away(&self.records)?;
        self.inner.records(slug)
    }
    fn record(&self, slug: &str, key: &str) -> Result<Option<Record>> {
        self.inner.record(slug, key)
    }
    fn note(&self, slug: &str, path: &str) -> Result<NoteBytes> {
        away(&self.note)?;
        self.inner.note(slug, path)
    }
    fn note_exists(&self, slug: &str, path: &str) -> Result<Option<bool>> {
        self.inner.note_exists(slug, path)
    }
    fn digests(&self, slug: &str) -> Result<Vec<notedown_cloud::appdata::Digest>> {
        self.inner.digests(slug)
    }
    fn journal(&self, slug: &str, from: u64, limit: usize) -> Result<JournalBatch> {
        self.inner.journal(slug, from, limit)
    }
}

fn errors_file(local: &Path) -> String {
    std::fs::read_to_string(local.join("pipeline").join(SLUG).join("errors.jsonl"))
        .unwrap_or_default()
}

/// One part of the source fails for the account while the account itself is listed: the
/// cursor stays, nothing is deleted, no try is counted; after it is back, all is read.
fn a_failing_part_keeps_the_cursor(fail_records: bool) {
    let (d, local) = two_folders();
    let key1 = d.put("test1", "NOTE/Note");
    let records = Arc::new(AtomicBool::new(false));
    let note = Arc::new(AtomicBool::new(false));
    let mut s = load(local.path());
    s.source = Arc::new(Partial {
        inner: FolderSource::new(&d.data),
        records: records.clone(),
        note: note.clone(),
    });
    let said = Said::default();
    let clock = TestClock::at(now_ms());
    let mut w = Worker::new(&s, Box::new(engine_reading("hello")))
        .with_clock(Arc::new(clock.clone()))
        .with_messages(said.sink());
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    assert_eq!(log(local.path()).len(), 1);
    let first = cursor::read(local.path(), SLUG).unwrap();
    assert_eq!(first, last_seq(&d.data));

    let key2 = d.put("test2", "NOTE/Note");
    // The clock past the record's mtime, so test2 is due and not settling.
    clock.set(now_ms());
    let flag = if fail_records { &records } else { &note };
    flag.store(true, Ordering::SeqCst);
    let state = TextStore::open(local.path(), SLUG)
        .unwrap()
        .note(&key1)
        .unwrap();
    w.run(stop_at(&clock, clock.now_ms() + 25 * 60_000))
        .unwrap();
    let lines = log(local.path());
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines.iter().all(|l| l.change != LineChange::NoteDeleted));
    let store = TextStore::open(local.path(), SLUG).unwrap();
    assert_eq!(store.note(&key1).unwrap(), state, "the store is unchanged");
    assert!(store.note(&key2).unwrap().is_none(), "no state, no try");
    assert_eq!(errors_file(local.path()), "", "no try is counted");
    assert_eq!(
        cursor::read(local.path(), SLUG).unwrap(),
        first,
        "{:?}",
        said.lines()
    );
    assert_eq!(said.count("the share is away"), 1, "{:?}", said.lines());

    flag.store(false, Ordering::SeqCst);
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    let store = TextStore::open(local.path(), SLUG).unwrap();
    assert!(store.note(&key2).unwrap().is_some(), "test2 is read");
    assert_eq!(log(local.path()).len(), 2);
    assert_eq!(errors_file(local.path()), "");
    assert_eq!(cursor::read(local.path(), SLUG).unwrap(), last_seq(&d.data));
}

#[test]
fn failing_records_keep_the_cursor_and_count_no_try() {
    a_failing_part_keeps_the_cursor(true);
}

#[test]
fn a_failing_note_read_keeps_the_cursor_and_counts_no_try() {
    a_failing_part_keeps_the_cursor(false);
}

#[test]
fn a_corrupt_cursor_is_read_again_from_the_start() {
    let (d, local) = two_folders();
    d.put("test1", "NOTE/Note");
    let file = cursor::path(local.path(), SLUG);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "{").unwrap();
    let said = Said::default();
    let clock = TestClock::at(now_ms());
    let mut w = Worker::new(&load(local.path()), Box::new(engine_reading("hello")))
        .with_clock(Arc::new(clock.clone()))
        .with_messages(said.sink());
    w.run(stop_at(&clock, clock.now_ms() + 5_000)).unwrap();
    assert_eq!(
        said.count(&format!("{SLUG}.cursor")),
        1,
        "{:?}",
        said.lines()
    );
    assert_eq!(log(local.path()).len(), 1);
    assert_eq!(
        cursor::read(local.path(), SLUG).unwrap(),
        last_seq(&d.data),
        "the cursor is written again"
    );
}

// Owners, slug reuse and `account.removed` (the note text on the Mac is deleted only for a
// removed or replaced account).

const FIRST_ID: u64 = 1_000_001;
const SECOND_ID: u64 = 1_000_002;

fn store_dir(local: &Path, slug: &str) -> PathBuf {
    local.join("pipeline").join(slug)
}

/// The user id in `<local>/pipeline/<slug>/owner.json`, `None` without one. Creates nothing.
fn owner_of(local: &Path, slug: &str) -> Option<u64> {
    let bytes = std::fs::read(store_dir(local, slug).join("owner.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    Some(v["userId"].as_u64().unwrap())
}

fn create_account(data: &Path, slug: &str) {
    Accounts::new(data)
        .create(slug, &format!("{slug}@example.org"), "secret")
        .unwrap();
}

fn remove_account(data: &Path, slug: &str) {
    Accounts::new(data)
        .remove(slug, &Journal::open(data).unwrap())
        .unwrap();
}

/// Puts fixture `name` into `folder` of account `slug`; returns the note key.
fn put_into(data: &Path, slug: &str, name: &str, folder: &str) -> String {
    let journal = Journal::open(data).unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/notes")
        .join(format!("{name}.note"));
    notedown_cloud::put::put(data, slug, &journal, &src, folder, None).unwrap();
    let path = format!("{folder}/{name}.note");
    FolderSource::new(data)
        .records(slug)
        .unwrap()
        .records
        .into_iter()
        .find(|(_, r)| r.record.path == path)
        .unwrap_or_else(|| panic!("no page record for {path}"))
        .0
}

fn tombstones(local: &Path) -> Vec<String> {
    std::fs::read_dir(local.join("pipeline"))
        .map(|e| {
            e.flatten()
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .filter(|n| n.starts_with(".removed-"))
                .collect()
        })
        .unwrap_or_default()
}

fn removed_message(slug: &str) -> String {
    format!("notedown pipeline: account {slug} was removed on the server; its note text is deleted")
}

/// A continuous worker on `s` with a test clock and the messages in `said`.
fn continuous(s: &Setup, said: &Said) -> (Worker, TestClock) {
    let clock = TestClock::at(now_ms());
    let w = Worker::new(s, Box::new(engine_reading("hello")))
        .with_clock(Arc::new(clock.clone()))
        .with_messages(said.sink());
    (w, clock)
}

/// Whether permissions bind this process (they do not for root).
fn permissions_bind(file: &Path) -> bool {
    std::fs::read(file).is_err()
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn a_removed_account_loses_its_text_by_the_event() {
    let (d, local) = two_folders();
    let key = d.put("test1", "NOTE/Note");
    let said = Said::default();
    let (mut w, clock) = continuous(&load(local.path()), &said);
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    let dir = store_dir(local.path(), SLUG);
    assert!(
        TextStore::open(local.path(), SLUG)
            .unwrap()
            .note(&key)
            .unwrap()
            .is_some()
    );
    assert_eq!(owner_of(local.path(), SLUG), Some(FIRST_ID));

    remove_account(&d.data, SLUG);
    let server = tree(&d.data);
    w.run(stop_at(&clock, clock.now_ms() + 5_000)).unwrap();
    assert!(!dir.exists(), "{:?}", said.lines());
    assert_eq!(said.count(&removed_message(SLUG)), 1, "{:?}", said.lines());
    assert!(tombstones(local.path()).is_empty());
    assert_eq!(cursor::read(local.path(), SLUG).unwrap(), last_seq(&d.data));
    assert_eq!(tree(&d.data), server, "the server folder is untouched");
}

#[test]
fn a_failed_removal_holds_the_cursor_and_is_tried_again() {
    let (d, local) = two_folders();
    d.put("test1", "NOTE/Note");
    let s = load(local.path());
    let said = Said::default();
    let (mut w, clock) = continuous(&s, &said);
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    let dir = store_dir(local.path(), SLUG);
    let owner = dir.join("owner.json");
    assert!(owner.is_file());

    remove_account(&d.data, SLUG);
    let removal = last_seq(&d.data);
    // The owner cannot be read, so the removal cannot be checked; the pipeline folder itself
    // stays writable, so the cursor could move.
    set_mode(&owner, 0o000);
    if !permissions_bind(&owner) {
        set_mode(&owner, 0o644);
        eprintln!("skipped: running as root, an unreadable file can be read");
        return;
    }
    w.run(stop_at(&clock, clock.now_ms() + 5_000)).unwrap();
    assert!(dir.join("notes").is_dir(), "the store stays");
    assert!(
        cursor::read(local.path(), SLUG).unwrap() < removal,
        "{:?}",
        said.lines()
    );
    let failed = format!("notedown pipeline: account {SLUG} was removed on the server");
    let tries = said.count(&failed);
    assert_eq!(tries, 1, "said once: {:?}", said.lines());
    assert_eq!(said.count(&removed_message(SLUG)), 0);

    // A restart reads the removal again from the cursor.
    let (mut w, clock) = continuous(&s, &said);
    let start = clock.now_ms();
    let c = clock.clone();
    let mid: Arc<Mutex<Option<(u64, bool)>>> = Arc::default();
    let seen = mid.clone();
    let (local_path, owner_path) = (local.path().to_path_buf(), owner.clone());
    w.run(move || {
        let now = c.now_ms();
        let mut m = seen.lock().unwrap();
        if now >= start + 5_000 && m.is_none() {
            *m = Some((
                cursor::read(&local_path, SLUG).unwrap(),
                store_dir(&local_path, SLUG).join("notes").is_dir(),
            ));
            set_mode(&owner_path, 0o644);
        }
        now >= start + 10_000
    })
    .unwrap();
    let (held, kept) = mid.lock().unwrap().unwrap();
    assert!(held < removal, "the cursor is held after a restart");
    assert!(kept, "the store stays while the removal fails");
    assert!(!dir.exists(), "{:?}", said.lines());
    assert_eq!(said.count(&removed_message(SLUG)), 1, "{:?}", said.lines());
    assert_eq!(cursor::read(local.path(), SLUG).unwrap(), last_seq(&d.data));
}

#[test]
fn an_empty_server_folder_deletes_nothing() {
    let (d, local) = two_folders();
    let key = d.put("test1", "NOTE/Note");
    let said = Said::default();
    let (mut w, clock) = continuous(&load(local.path()), &said);
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    let dir = store_dir(local.path(), SLUG);
    let before = tree(&dir.join("notes"));
    let lines = log(local.path()).len();

    // Like a share that is not mounted: the accounts are away, then the whole folder.
    let accounts = d.data.join("accounts");
    let away = d.data.with_extension("away");
    std::fs::rename(&accounts, &away).unwrap();
    std::fs::create_dir(&accounts).unwrap();
    w.run(stop_at(&clock, clock.now_ms() + 25 * 60_000))
        .unwrap();
    std::fs::remove_dir(&accounts).unwrap();
    let server = d.data.with_extension("server");
    std::fs::rename(&d.data, &server).unwrap();
    w.run(stop_at(&clock, clock.now_ms() + 25 * 60_000))
        .unwrap();
    std::fs::rename(&server, &d.data).unwrap();
    std::fs::rename(&away, &accounts).unwrap();

    assert_eq!(tree(&dir.join("notes")), before, "{:?}", said.lines());
    assert_eq!(log(local.path()).len(), lines);
    assert!(
        log(local.path())
            .iter()
            .all(|l| l.change != LineChange::NoteDeleted)
    );
    let state = TextStore::open(local.path(), SLUG)
        .unwrap()
        .note(&key)
        .unwrap()
        .unwrap();
    assert!(!state.deleted);
    assert_eq!(owner_of(local.path(), SLUG), Some(FIRST_ID));
}

#[test]
fn a_reused_slug_never_shows_the_old_text() {
    let (d, local) = two_folders();
    let key = d.put("test1", "NOTE/Note");
    let s = load(local.path());
    Worker::new(&s, Box::new(engine_reading("hello")))
        .run_once(true)
        .unwrap();
    assert_eq!(owner_of(local.path(), SLUG), Some(FIRST_ID));

    remove_account(&d.data, SLUG);
    create_account(&d.data, SLUG);
    // Past the removal, so only the owner check can act.
    cursor::write(local.path(), SLUG, last_seq(&d.data)).unwrap();
    let said = Said::default();
    let (mut w, clock) = continuous(&s, &said);
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    let store = TextStore::open(local.path(), SLUG).unwrap();
    assert!(store.note(&key).unwrap().is_none(), "{:?}", said.lines());
    assert!(store.notes().unwrap().is_empty());
    assert_eq!(store.last_seq().unwrap(), 0, "the old log is gone");
    assert_eq!(owner_of(local.path(), SLUG), Some(SECOND_ID));
    let message = format!(
        "notedown pipeline: account {SLUG} belongs to a new user id ({SECOND_ID}, was \
         {FIRST_ID}); its old note text is deleted"
    );
    assert_eq!(said.count(&message), 1, "{:?}", said.lines());
    assert!(tombstones(local.path()).is_empty());
}

#[test]
fn a_reused_slug_is_checked_by_run_once_too() {
    let (d, local) = two_folders();
    let key = d.put("test1", "NOTE/Note");
    let s = load(local.path());
    Worker::new(&s, Box::new(engine_reading("hello")))
        .run_once(true)
        .unwrap();
    remove_account(&d.data, SLUG);
    create_account(&d.data, SLUG);
    Worker::new(&s, Box::new(engine_reading("hello")))
        .with_messages(Said::default().sink())
        .run_once(true)
        .unwrap();
    let store = TextStore::open(local.path(), SLUG).unwrap();
    assert!(store.note(&key).unwrap().is_none());
    assert_eq!(owner_of(local.path(), SLUG), Some(SECOND_ID));
}

#[test]
fn an_old_removal_does_not_delete_a_reused_slug() {
    let (d, local) = two_folders();
    remove_account(&d.data, SLUG);
    create_account(&d.data, SLUG);
    let key = d.put("test1", "NOTE/Note");
    let said = Said::default();
    let (mut w, clock) = continuous(&load(local.path()), &said);
    w.run(stop_at(&clock, clock.now_ms() + 5_000)).unwrap();
    assert_eq!(owner_of(local.path(), SLUG), Some(SECOND_ID));
    let store = TextStore::open(local.path(), SLUG).unwrap();
    assert!(store.note(&key).unwrap().is_some(), "{:?}", said.lines());
    assert_eq!(said.count(&removed_message(SLUG)), 0);
    assert_eq!(cursor::read(local.path(), SLUG).unwrap(), last_seq(&d.data));
}

#[test]
fn a_removal_record_of_a_live_account_deletes_nothing() {
    use notedown_cloud::journal::{Event, EventBody, Origin, new_op_id};
    let (d, local) = two_folders();
    let key = d.put("test1", "NOTE/Note");
    let said = Said::default();
    let (mut w, clock) = continuous(&load(local.path()), &said);
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    // A crash between the record and the rename: the account lives on with the same id.
    Journal::open(&d.data)
        .unwrap()
        .append(&mut [Event::new(
            SLUG,
            Origin::Cli,
            None,
            &new_op_id(),
            EventBody::AccountRemoved { user_id: FIRST_ID },
        )])
        .unwrap();
    w.run(stop_at(&clock, clock.now_ms() + 5_000)).unwrap();
    let store = TextStore::open(local.path(), SLUG).unwrap();
    assert!(store.note(&key).unwrap().is_some(), "{:?}", said.lines());
    assert_eq!(owner_of(local.path(), SLUG), Some(FIRST_ID));
    assert_eq!(said.count(&removed_message(SLUG)), 0);
    assert_eq!(cursor::read(local.path(), SLUG).unwrap(), last_seq(&d.data));
}

#[test]
fn a_store_without_owner_is_not_adopted_on_two_machines() {
    let (d, local) = two_folders();
    let key1 = d.put("test1", "NOTE/Note");
    let s = load(local.path());
    Worker::new(&s, Box::new(engine_reading("hello")))
        .run_once(true)
        .unwrap();
    let dir = store_dir(local.path(), SLUG);
    std::fs::remove_file(dir.join("owner.json")).unwrap();
    let key2 = d.put("test2", "NOTE/Note");
    let said = Said::default();
    let (mut w, clock) = continuous(&s, &said);
    w.run(stop_at(&clock, clock.now_ms() + 25 * 60_000))
        .unwrap();
    let message = format!(
        "notedown pipeline: {} has note text but no owner; copy it from a pipeline that ran \
         this version, or delete it to read everything again",
        dir.display()
    );
    assert_eq!(said.count(&message), 1, "{:?}", said.lines());
    assert!(!dir.join("owner.json").exists());
    let store = TextStore::open(local.path(), SLUG).unwrap();
    assert!(store.note(&key1).unwrap().is_some());
    assert!(
        store.note(&key2).unwrap().is_none(),
        "the account is skipped"
    );
    assert_eq!(log(local.path()).len(), 1);
}

#[test]
fn one_machine_sets_the_owner_of_an_existing_store() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    let s = load(&d.data);
    Worker::new(&s, Box::new(engine_reading("hello")))
        .run_once(true)
        .unwrap();
    let owner = store_dir(&d.data, SLUG).join("owner.json");
    std::fs::remove_file(&owner).unwrap();
    Worker::new(&s, Box::new(engine_reading("hello")))
        .run_once(true)
        .unwrap();
    assert_eq!(owner_of(&d.data, SLUG), Some(FIRST_ID));
    assert!(d.store().note(&key).unwrap().is_some());
    assert_eq!(log(&d.data).len(), 1);
}

#[test]
fn one_machine_keeps_an_unowned_store_of_a_reused_slug() {
    // A store from before owners, of the account that has the slug now. An old removal of
    // the slug (another user id) is read again from the start: the store stays.
    let d = setup();
    remove_account(&d.data, SLUG);
    create_account(&d.data, SLUG);
    let key = d.put("test1", "NOTE/Note");
    let s = load(&d.data);
    Worker::new(&s, Box::new(engine_reading("hello")))
        .run_once(true)
        .unwrap();
    std::fs::remove_file(store_dir(&d.data, SLUG).join("owner.json")).unwrap();
    let said = Said::default();
    let (mut w, clock) = continuous(&s, &said);
    w.run(stop_at(&clock, clock.now_ms() + 5_000)).unwrap();
    assert!(
        d.store().note(&key).unwrap().is_some(),
        "{:?}",
        said.lines()
    );
    assert_eq!(owner_of(&d.data, SLUG), Some(SECOND_ID));
    assert_eq!(said.count(&removed_message(SLUG)), 0);
}

#[test]
fn one_machine_deletes_an_unowned_store_of_a_removed_account() {
    // `Accounts::remove` could not delete the note text; the store has no owner yet.
    let d = setup();
    d.put("test1", "NOTE/Note");
    let s = load(&d.data);
    let said = Said::default();
    let (mut w, clock) = continuous(&s, &said);
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    let dir = store_dir(&d.data, SLUG);
    std::fs::remove_file(dir.join("owner.json")).unwrap();
    let kept = d.data.join("kept");
    std::fs::rename(&dir, &kept).unwrap();
    remove_account(&d.data, SLUG);
    std::fs::rename(&kept, &dir).unwrap();
    w.run(stop_at(&clock, clock.now_ms() + 5_000)).unwrap();
    assert!(!dir.exists(), "{:?}", said.lines());
    assert_eq!(said.count(&removed_message(SLUG)), 1, "{:?}", said.lines());
}

#[test]
fn a_leftover_tombstone_is_swept_at_start() {
    let (d, local) = two_folders();
    d.put("test1", "NOTE/Note");
    let tomb = local.path().join("pipeline").join(".removed-x-1");
    std::fs::create_dir_all(tomb.join("notes")).unwrap();
    std::fs::write(tomb.join("notes").join("a.json"), "{}").unwrap();
    let said = Said::default();
    let (mut w, clock) = continuous(&load(local.path()), &said);
    w.run(|| true).unwrap();
    assert!(!tomb.exists());
    assert!(local.path().join("pipeline").join(".writer.lock").exists());
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    assert_eq!(log(local.path()).len(), 1);
}

#[test]
fn removing_an_account_on_one_machine_leaves_the_worker_running() {
    let d = setup();
    create_account(&d.data, "anna");
    d.put("test1", "NOTE/Note");
    put_into(&d.data, "anna", "test1", "NOTE/Note");
    let said = Said::default();
    let (mut w, clock) = continuous(&load(&d.data), &said);
    let start = clock.now_ms();
    let c = clock.clone();
    let data = d.data.clone();
    let step = Mutex::new(0);
    w.run(move || {
        let now = c.now_ms();
        let mut step = step.lock().unwrap();
        if *step == 0 && now >= start + 5_000 {
            remove_account(&data, SLUG);
            *step = 1;
        } else if *step == 1 && now >= start + 6_000 {
            put_into(&data, "anna", "test2", "NOTE/Note");
            *step = 2;
        }
        now >= start + 15 * 60_000
    })
    .unwrap();
    assert!(!store_dir(&d.data, SLUG).exists(), "{:?}", said.lines());
    let anna = TextStore::open(&d.data, "anna").unwrap();
    assert_eq!(anna.notes().unwrap().len(), 2, "{:?}", said.lines());
    assert!(said.lines().is_empty(), "{:?}", said.lines());
    assert_eq!(owner_of(&d.data, "anna"), Some(SECOND_ID));
}

/// Per account: the live note keys, and per note the block texts of each page in order.
type Texts = Vec<(String, Vec<Vec<String>>)>;

fn texts_of(local: &Path, slug: &str) -> Texts {
    let store = TextStore::open(local, slug).unwrap();
    store
        .notes()
        .unwrap()
        .into_iter()
        .filter(|n| !n.deleted)
        .map(|n| {
            let pages = n
                .pages
                .iter()
                .map(|p| p.blocks.iter().map(|b| b.text.clone()).collect())
                .collect();
            (n.key, pages)
        })
        .collect()
}

#[test]
fn catch_up_gives_the_same_text_as_always_on() {
    use crate::worker::with_parts;
    let d = setup();
    create_account(&d.data, "anna");
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    point_at(a.path(), &d.data);
    point_at(b.path(), &d.data);
    let said = Said::default();
    let (mut mac_a, clock) = continuous(&load(a.path()), &said);
    let mut step = |f: &dyn Fn()| {
        f();
        clock.set(clock.now_ms().max(now_ms()));
        mac_a.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    };
    let key1 = std::cell::RefCell::new(String::new());
    step(&|| *key1.borrow_mut() = d.put("test1", "NOTE/Note"));
    step(&|| {
        d.edit_record(&key1.borrow(), |r| {
            r.pages[0] = with_parts(&r.pages[0], Some("0123456789abcdef"), None, None);
        })
    });
    step(&|| {
        d.put("test2", "NOTE/Note");
    });
    let old = std::cell::RefCell::new(String::new());
    step(&|| *old.borrow_mut() = put_into(&d.data, "anna", "test1", "NOTE/Note/Old"));
    assert_eq!(texts_of(a.path(), "anna").len(), 1);
    step(&|| {
        remove_account(&d.data, "anna");
        create_account(&d.data, "anna");
        put_into(&d.data, "anna", "test2", "NOTE/Note/New");
    });

    let (mut mac_b, clock_b) = continuous(&load(b.path()), &said);
    clock_b.set(now_ms());
    mac_b.run(stop_at(&clock_b, clock_b.now_ms() + 1)).unwrap();
    for slug in [SLUG, "anna"] {
        let (ta, tb) = (texts_of(a.path(), slug), texts_of(b.path(), slug));
        assert!(!ta.is_empty());
        assert_eq!(ta, tb, "{slug}: {:?}", said.lines());
        assert!(owner_of(a.path(), slug).is_some());
        assert_eq!(owner_of(a.path(), slug), owner_of(b.path(), slug));
    }
    assert_eq!(texts_of(a.path(), "anna").len(), 1, "only the new note");
    let anna = TextStore::open(a.path(), "anna").unwrap();
    assert!(
        anna.note(&old.borrow()).unwrap().is_none(),
        "no trace of the old account's note"
    );
}

/// A folder source whose `accounts()` gives an old list once when one is set: the list a Mac
/// read just before the server removed an account.
struct Stale {
    inner: FolderSource,
    old: Arc<Mutex<Option<Vec<ServerAccount>>>>,
}

impl ServerData for Stale {
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn accounts(&self) -> Result<Vec<ServerAccount>> {
        match self.old.lock().unwrap().take() {
            Some(old) => Ok(old),
            None => self.inner.accounts(),
        }
    }
    fn records(&self, slug: &str) -> Result<Records> {
        self.inner.records(slug)
    }
    fn record(&self, slug: &str, key: &str) -> Result<Option<Record>> {
        self.inner.record(slug, key)
    }
    fn note(&self, slug: &str, path: &str) -> Result<NoteBytes> {
        self.inner.note(slug, path)
    }
    fn note_exists(&self, slug: &str, path: &str) -> Result<Option<bool>> {
        self.inner.note_exists(slug, path)
    }
    fn digests(&self, slug: &str) -> Result<Vec<notedown_cloud::appdata::Digest>> {
        self.inner.digests(slug)
    }
    fn journal(&self, slug: &str, from: u64, limit: usize) -> Result<JournalBatch> {
        self.inner.journal(slug, from, limit)
    }
}

#[test]
fn a_removal_read_after_an_old_account_list_still_deletes() {
    let (d, local) = two_folders();
    d.put("test1", "NOTE/Note");
    let old = Arc::new(Mutex::new(None));
    let mut s = load(local.path());
    s.source = Arc::new(Stale {
        inner: FolderSource::new(&d.data),
        old: old.clone(),
    });
    let said = Said::default();
    let (mut w, clock) = continuous(&s, &said);
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    // The Mac lists the accounts, then the server removes one, then the Mac reads the journal.
    let before = FolderSource::new(&d.data).accounts().unwrap();
    remove_account(&d.data, SLUG);
    *old.lock().unwrap() = Some(before);
    w.run(stop_at(&clock, clock.now_ms() + 5_000)).unwrap();
    assert!(old.lock().unwrap().is_none(), "the old list was given");
    assert!(
        !store_dir(local.path(), SLUG).exists(),
        "{:?}",
        said.lines()
    );
    assert_eq!(said.count(&removed_message(SLUG)), 1, "{:?}", said.lines());
    assert_eq!(cursor::read(local.path(), SLUG).unwrap(), last_seq(&d.data));
}

/// Moves `<server>/accounts` away while `f` runs, like a share that is only half there.
fn without_accounts(data: &Path, f: impl FnOnce()) {
    let accounts = data.join("accounts");
    let away = data.join("accounts.away");
    std::fs::rename(&accounts, &away).unwrap();
    f();
    std::fs::rename(&away, &accounts).unwrap();
}

#[test]
fn a_removal_read_while_the_accounts_are_missing_waits() {
    let (d, local) = two_folders();
    d.put("test1", "NOTE/Note");
    let said = Said::default();
    let (mut w, clock) = continuous(&load(local.path()), &said);
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    let before = cursor::read(local.path(), SLUG).unwrap();
    remove_account(&d.data, SLUG);
    let dir = store_dir(local.path(), SLUG);
    without_accounts(&d.data, || {
        w.run(stop_at(&clock, clock.now_ms() + 25 * 60_000))
            .unwrap();
        assert!(dir.join("notes").is_dir(), "{:?}", said.lines());
        assert_eq!(cursor::read(local.path(), SLUG).unwrap(), before);
    });
    let away = said.count(&d.data.join("accounts").display().to_string());
    assert_eq!(away, 1, "said once: {:?}", said.lines());
    assert_eq!(said.count(&removed_message(SLUG)), 0);
    // Back: the removal is carried out as usual.
    w.run(stop_at(&clock, clock.now_ms() + 5_000)).unwrap();
    assert!(!dir.exists(), "{:?}", said.lines());
    assert_eq!(said.count(&removed_message(SLUG)), 1);
    assert_eq!(cursor::read(local.path(), SLUG).unwrap(), last_seq(&d.data));
}

#[test]
fn a_removal_record_of_a_live_account_read_while_the_accounts_are_missing_deletes_nothing() {
    use notedown_cloud::journal::{Event, EventBody, Origin, new_op_id};
    let (d, local) = two_folders();
    let key = d.put("test1", "NOTE/Note");
    let said = Said::default();
    let (mut w, clock) = continuous(&load(local.path()), &said);
    w.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    Journal::open(&d.data)
        .unwrap()
        .append(&mut [Event::new(
            SLUG,
            Origin::Cli,
            None,
            &new_op_id(),
            EventBody::AccountRemoved { user_id: FIRST_ID },
        )])
        .unwrap();
    without_accounts(&d.data, || {
        w.run(stop_at(&clock, clock.now_ms() + 5_000)).unwrap();
    });
    w.run(stop_at(&clock, clock.now_ms() + 5_000)).unwrap();
    let store = TextStore::open(local.path(), SLUG).unwrap();
    assert!(store.note(&key).unwrap().is_some(), "{:?}", said.lines());
    assert_eq!(owner_of(local.path(), SLUG), Some(FIRST_ID));
    assert_eq!(said.count(&removed_message(SLUG)), 0);
    assert_eq!(cursor::read(local.path(), SLUG).unwrap(), last_seq(&d.data));
}

#[test]
fn a_fresh_one_machine_folder_starts_and_waits_for_accounts() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().to_path_buf();
    let said = Said::default();
    let (mut w, clock) = continuous(&load(&data), &said);
    let start = clock.now_ms();
    let c = clock.clone();
    let d = data.clone();
    let made = AtomicBool::new(false);
    w.run(move || {
        let now = c.now_ms();
        if now >= start + 5_000 && !made.swap(true, Ordering::SeqCst) {
            create_account(&d, SLUG);
            put_into(&d, SLUG, "test1", "NOTE/Note");
        }
        now >= start + 10_000
    })
    .unwrap();
    assert_eq!(said.lines().len(), 1, "said once: {:?}", said.lines());
    assert_eq!(log(&data).len(), 1, "read once the account is there");
}

#[test]
fn a_tombstone_that_cannot_be_deleted_is_said_once() {
    let (d, local) = two_folders();
    d.put("test1", "NOTE/Note");
    let tomb = local.path().join("pipeline").join(".removed-x-1");
    let locked = tomb.join("notes");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::write(locked.join("a.json"), "{}").unwrap();
    set_mode(&locked, 0o555);
    if std::fs::write(locked.join("probe"), "").is_ok() {
        set_mode(&locked, 0o755);
        eprintln!("skipped: running as root, a read-only folder can be written");
        return;
    }
    let said = Said::default();
    let (mut w, clock) = continuous(&load(local.path()), &said);
    w.run(stop_at(&clock, clock.now_ms() + 5_000)).unwrap();
    w.run(stop_at(&clock, clock.now_ms() + 5_000)).unwrap();
    let still = tomb.exists();
    set_mode(&locked, 0o755);
    assert!(still);
    let n = said.count(&tomb.display().to_string());
    assert_eq!(n, 1, "said once: {:?}", said.lines());
    // Once it can be deleted, the next start does it.
    w.run(|| true).unwrap();
    assert!(!tomb.exists());
}
