//! The pipeline on a second machine: it reads the server's folder through `ServerData` and
//! keeps its own data (note text, cursors) in a local folder.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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
