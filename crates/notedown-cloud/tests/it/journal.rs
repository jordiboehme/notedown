//! Change journal: seq, rotation, cursors, reading, and the commit protocol with crash repair.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use notedown_cloud::Clock;
use notedown_cloud::auth::Accounts;
use notedown_cloud::journal::{
    Event, EventBody, FileKind, Journal, JournalOptions, Origin, new_op_id,
};
use notedown_cloud::store::{
    CrashPoint, PENDING_FILE, PendingOp, Recovery, Store, WirePath, recover_all,
};
use time::OffsetDateTime;

struct TestClock(AtomicI64);

impl Clock for TestClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp_nanos(self.0.load(Ordering::SeqCst) as i128 * 1_000_000)
            .unwrap()
    }
}

/// 2026-10-06T12:00:00Z
const T0: i64 = 1_791_288_000_000;

fn clock(ms: i64) -> Arc<TestClock> {
    Arc::new(TestClock(AtomicI64::new(ms)))
}

fn journal_with(dir: &Path, cap: u64, clock: Arc<TestClock>) -> Journal {
    Journal::open_with(
        dir,
        JournalOptions {
            max_file_bytes: cap,
            clock,
        },
    )
    .unwrap()
}

fn ev(n: u64) -> Event {
    Event::new(
        "jordi",
        Origin::Cli,
        None,
        &format!("op-{n}"),
        EventBody::FolderCreated {
            id: 1000 + n,
            path: format!("F{n}"),
        },
    )
}

fn all(j: &Journal) -> Vec<Event> {
    j.read_from(0).unwrap().map(|e| e.unwrap()).collect()
}

fn seqs(j: &Journal) -> Vec<u64> {
    all(j).iter().map(|e| e.seq).collect()
}

fn wp(s: &str) -> WirePath {
    WirePath::parse_named(s).unwrap()
}

// ---------------------------------------------------------------- journal

#[test]
fn seq_is_strictly_increasing_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let j = Journal::open(dir.path()).unwrap();
    assert_eq!(j.last_seq().unwrap(), 0);
    let mut batch = vec![ev(1), ev(2), ev(3)];
    assert_eq!(j.append(&mut batch).unwrap(), vec![1, 2, 3]);
    assert_eq!(batch[2].seq, 3);
    assert!(batch[0].ts > 0);
    drop(j);
    let j = Journal::open(dir.path()).unwrap();
    assert_eq!(j.last_seq().unwrap(), 3);
    assert_eq!(j.append(&mut [ev(4)]).unwrap(), vec![4]);
    assert_eq!(j.append(&mut []).unwrap(), Vec::<u64>::new());
    assert_eq!(seqs(&j), vec![1, 2, 3, 4]);
    // one file, named after its first seq, one line per event with a newline
    let files = j.files().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].1.file_name().unwrap().to_str().unwrap(),
        "journal-000000000001.jsonl"
    );
    let text = std::fs::read_to_string(&files[0].1).unwrap();
    assert_eq!(text.lines().count(), 4);
    assert!(text.ends_with('\n'));
}

#[test]
fn concurrent_handles_never_duplicate_or_lose_a_seq() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let threads: Vec<_> = (0..4)
        .map(|t| {
            let path = path.clone();
            std::thread::spawn(move || {
                // a separate handle per thread, as separate processes have
                let j = journal_with(&path, 4096, clock(T0));
                let mut got = Vec::new();
                for i in 0..40 {
                    let mut batch = vec![ev(t * 1000 + i), ev(t * 1000 + i)];
                    let s = j.append(&mut batch).unwrap();
                    assert_eq!(s[1], s[0] + 1, "a batch is contiguous");
                    got.extend(s);
                }
                got
            })
        })
        .collect();
    let mut got: Vec<u64> = threads
        .into_iter()
        .flat_map(|t| t.join().unwrap())
        .collect();
    got.sort();
    assert_eq!(got, (1..=320).collect::<Vec<_>>());
    let j = Journal::open(dir.path()).unwrap();
    assert_eq!(seqs(&j), (1..=320).collect::<Vec<_>>());
    assert!(j.files().unwrap().len() > 1, "the small cap rotated");
}

#[test]
fn rotation_by_size_never_splits_a_batch() {
    let dir = tempfile::tempdir().unwrap();
    // the exact length of one line (seq and ts included), from a scratch journal
    let scratch = tempfile::tempdir().unwrap();
    let sj = journal_with(scratch.path(), 1 << 20, clock(T0));
    sj.append(&mut [ev(1)]).unwrap();
    let line = std::fs::metadata(&sj.files().unwrap()[0].1).unwrap().len();
    let j = journal_with(dir.path(), line * 2, clock(T0));
    j.append(&mut [ev(1)]).unwrap(); // file 1: 1 line, below the cap
    j.append(&mut [ev(2), ev(3), ev(4)]).unwrap(); // still file 1 (the cap is checked before)
    j.append(&mut [ev(5)]).unwrap(); // file 1 is over the cap: file 5
    j.append(&mut [ev(6)]).unwrap();
    j.append(&mut [ev(7)]).unwrap(); // file 5 has 2 lines, at the cap: file 7
    let names: Vec<u64> = j.files().unwrap().iter().map(|(f, _)| *f).collect();
    assert_eq!(names, vec![1, 5, 7]);
    let first = std::fs::read_to_string(&j.files().unwrap()[0].1).unwrap();
    assert_eq!(first.lines().count(), 4);
    assert_eq!(seqs(&j), (1..=7).collect::<Vec<_>>());
    // read_from across rotated files, from the middle of one
    let from: Vec<u64> = j.read_from(3).unwrap().map(|e| e.unwrap().seq).collect();
    assert_eq!(from, vec![3, 4, 5, 6, 7]);
    let from: Vec<u64> = j.read_from(6).unwrap().map(|e| e.unwrap().seq).collect();
    assert_eq!(from, vec![6, 7]);
    assert_eq!(j.read_from(8).unwrap().count(), 0);
    // a restart continues the newest file
    let j = journal_with(dir.path(), line * 2, clock(T0));
    assert_eq!(j.last_seq().unwrap(), 7);
}

#[test]
fn rotation_at_a_month_change_utc() {
    let dir = tempfile::tempdir().unwrap();
    // 2026-10-31T23:59:59.000Z
    let c = clock(1_793_491_199_000);
    let j = journal_with(dir.path(), 1 << 30, c.clone());
    j.append(&mut [ev(1), ev(2)]).unwrap();
    c.0.fetch_add(500, Ordering::SeqCst);
    j.append(&mut [ev(3)]).unwrap(); // still October
    c.0.fetch_add(1_000, Ordering::SeqCst); // 2026-11-01T00:00:00.500Z
    j.append(&mut [ev(4)]).unwrap();
    j.append(&mut [ev(5)]).unwrap();
    let names: Vec<u64> = j.files().unwrap().iter().map(|(f, _)| *f).collect();
    assert_eq!(names, vec![1, 4]);
    assert_eq!(seqs(&j), vec![1, 2, 3, 4, 5]);
}

#[test]
fn a_torn_last_line_is_ignored_and_cut_before_the_next_append() {
    let dir = tempfile::tempdir().unwrap();
    let j = Journal::open(dir.path()).unwrap();
    j.append(&mut [ev(1), ev(2)]).unwrap();
    let file = j.files().unwrap()[0].1.clone();
    let mut text = std::fs::read_to_string(&file).unwrap();
    text.push_str("{\"seq\":3,\"ts\":17912");
    std::fs::write(&file, &text).unwrap();
    assert_eq!(seqs(&j), vec![1, 2]);
    assert_eq!(j.last_seq().unwrap(), 2);
    assert_eq!(j.append(&mut [ev(3)]).unwrap(), vec![3]);
    let text = std::fs::read_to_string(&file).unwrap();
    assert_eq!(text.lines().count(), 3);
    assert!(!text.contains("17912,"), "{text}");
    assert_eq!(seqs(&j), vec![1, 2, 3]);
}

// ---------------------------------------------------------------- tail reader

/// The seqs of the next batch of `tail`; every line must be readable.
fn batch(tail: &mut notedown_cloud::journal::JournalTail) -> Vec<u64> {
    tail.next_batch()
        .unwrap()
        .into_iter()
        .map(|e| e.unwrap().seq)
        .collect()
}

/// The length of one event line, from a scratch journal.
fn line_len() -> u64 {
    let scratch = tempfile::tempdir().unwrap();
    let sj = journal_with(scratch.path(), 1 << 20, clock(T0));
    sj.append(&mut [ev(1)]).unwrap();
    std::fs::metadata(&sj.files().unwrap()[0].1).unwrap().len()
}

#[test]
fn a_tail_returns_each_append_once_and_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let j = Journal::open(dir.path()).unwrap();
    let mut tail = j.tail(1);
    assert!(batch(&mut tail).is_empty());
    j.append(&mut [ev(1), ev(2)]).unwrap();
    assert_eq!(batch(&mut tail), vec![1, 2]);
    assert!(batch(&mut tail).is_empty());
    j.append(&mut [ev(3)]).unwrap();
    j.append(&mut [ev(4)]).unwrap();
    assert_eq!(batch(&mut tail), vec![3, 4]);
    assert_eq!(tail.next_seq(), 5);
    // A tail from the middle skips the older events.
    assert_eq!(batch(&mut j.tail(3)), vec![3, 4]);
    assert!(batch(&mut j.tail(5)).is_empty());
}

#[test]
fn a_tail_reads_a_torn_line_only_once_it_is_complete() {
    let dir = tempfile::tempdir().unwrap();
    let j = Journal::open(dir.path()).unwrap();
    j.append(&mut [ev(1)]).unwrap();
    let mut tail = j.tail(1);
    assert_eq!(batch(&mut tail), vec![1]);
    let file = j.files().unwrap()[0].1.clone();
    let one = std::fs::read(&file).unwrap();
    j.append(&mut [ev(2)]).unwrap();
    let full = std::fs::read(&file).unwrap();
    let half = one.len() + (full.len() - one.len()) / 2;
    std::fs::write(&file, &full[..half]).unwrap();
    assert!(batch(&mut tail).is_empty());
    std::fs::write(&file, &full[..full.len() - 1]).unwrap();
    assert!(batch(&mut tail).is_empty());
    std::fs::write(&file, &full).unwrap();
    assert_eq!(batch(&mut tail), vec![2]);
    assert!(batch(&mut tail).is_empty());
}

/// Lines added to a file after the tail read it, and a rotation right after: the batch has
/// both, in order.
#[test]
fn a_tail_follows_a_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let j = journal_with(dir.path(), line_len() * 2, clock(T0));
    j.append(&mut [ev(1)]).unwrap();
    let mut tail = j.tail(1);
    assert_eq!(batch(&mut tail), vec![1]);
    j.append(&mut [ev(2)]).unwrap(); // still file 1 (the cap is checked before)
    j.append(&mut [ev(3)]).unwrap(); // file 3
    assert_eq!(j.files().unwrap().len(), 2);
    assert_eq!(batch(&mut tail), vec![2, 3]);
    for n in 4..=9 {
        j.append(&mut [ev(n)]).unwrap();
    }
    assert!(j.files().unwrap().len() > 3);
    assert_eq!(batch(&mut tail), (4..=9).collect::<Vec<_>>());
    assert!(batch(&mut tail).is_empty());
}

/// The file the tail reads is removed: it starts over as `read_from` does from the next seq,
/// so nothing comes twice; the events of the removed file are gone, as for `read_from`.
#[test]
fn a_tail_whose_file_vanished_starts_over() {
    let dir = tempfile::tempdir().unwrap();
    let j = journal_with(dir.path(), line_len() * 2, clock(T0));
    j.append(&mut [ev(1)]).unwrap();
    let mut tail = j.tail(1);
    assert_eq!(batch(&mut tail), vec![1]);
    j.append(&mut [ev(2)]).unwrap();
    j.append(&mut [ev(3)]).unwrap();
    let files = j.files().unwrap();
    assert_eq!(files.len(), 2);
    std::fs::remove_file(&files[0].1).unwrap();
    assert_eq!(batch(&mut tail), vec![3]);
    j.append(&mut [ev(4)]).unwrap();
    assert_eq!(batch(&mut tail), vec![4]);
}

/// The file the tail reads gets shorter: the tail starts over and returns only seqs it did
/// not return yet. Lines written again under a seq it returned already are not returned.
#[test]
fn a_tail_whose_file_was_cut_starts_over_without_repeats() {
    let dir = tempfile::tempdir().unwrap();
    let j = Journal::open(dir.path()).unwrap();
    j.append(&mut [ev(1), ev(2)]).unwrap();
    let mut tail = j.tail(1);
    assert_eq!(batch(&mut tail), vec![1, 2]);
    let file = j.files().unwrap()[0].1.clone();
    let text = std::fs::read(&file).unwrap();
    std::fs::write(&file, b"").unwrap();
    assert!(batch(&mut tail).is_empty());
    std::fs::write(&file, &text).unwrap();
    j.append(&mut [ev(3)]).unwrap();
    assert_eq!(batch(&mut tail), vec![3]);
    // Cut back to the first line: the next append reuses seq 2 and 3, which the tail
    // returned already; seq 4 is new.
    let first = text.iter().position(|b| *b == b'\n').unwrap() + 1;
    std::fs::write(&file, &text[..first]).unwrap();
    // Longer lines than before, so the old offset falls inside a line.
    let mut again = [ev_long(5, 40), ev_long(6, 40), ev_long(7, 40)];
    assert_eq!(j.append(&mut again).unwrap(), vec![2, 3, 4]);
    assert_eq!(batch(&mut tail), vec![4]);
}

/// An event whose path is `len` characters long, for lines of another length.
fn ev_long(n: u64, len: usize) -> Event {
    Event::new(
        "jordi",
        Origin::Cli,
        None,
        &format!("op-{n}"),
        EventBody::FolderCreated {
            id: 1000 + n,
            path: format!("F{n}-{}", "x".repeat(len)),
        },
    )
}

/// A journal file with seqs 1 to 12 and short lines, made in another folder.
fn short_lines_file() -> (tempfile::TempDir, std::path::PathBuf) {
    let other = tempfile::tempdir().unwrap();
    let oj = Journal::open(other.path()).unwrap();
    for n in 1..=12 {
        oj.append(&mut [ev(n)]).unwrap();
    }
    let file = oj.files().unwrap()[0].1.clone();
    (other, file)
}

/// The file the tail reads is replaced by another one with other line lengths (a copy over
/// it, a restore): the tail starts over, and the batch is exactly the seqs it did not return.
#[test]
fn a_tail_whose_file_was_replaced_starts_over() {
    for in_place in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(dir.path()).unwrap();
        j.append(&mut [ev_long(1, 300), ev_long(2, 300)]).unwrap();
        let mut tail = j.tail(1);
        assert_eq!(batch(&mut tail), vec![1, 2]);
        let file = j.files().unwrap()[0].1.clone();
        let offset = std::fs::metadata(&file).unwrap().len();
        let (_other, replacement) = short_lines_file();
        assert!(std::fs::metadata(&replacement).unwrap().len() >= offset);
        if in_place {
            // Same file, other bytes.
            std::fs::write(&file, std::fs::read(&replacement).unwrap()).unwrap();
        } else {
            std::fs::rename(&replacement, &file).unwrap();
        }
        let items = tail.next_batch().unwrap();
        let seqs: Vec<u64> = items
            .into_iter()
            .map(|e| e.unwrap_or_else(|e| panic!("in place {in_place}: {e}")).seq)
            .collect();
        assert_eq!(seqs, (3..=12).collect::<Vec<_>>(), "in place {in_place}");
        assert!(batch(&mut tail).is_empty());
    }
}

#[test]
fn a_tail_returns_a_bad_line_once() {
    let dir = tempfile::tempdir().unwrap();
    let j = Journal::open(dir.path()).unwrap();
    j.append(&mut [ev(1)]).unwrap();
    let file = j.files().unwrap()[0].1.clone();
    let mut text = std::fs::read(&file).unwrap();
    text.extend_from_slice(b"not json\n");
    std::fs::write(&file, &text).unwrap();
    let mut tail = j.tail(1);
    let items = tail.next_batch().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].as_ref().unwrap().seq, 1);
    assert!(items[1].is_err());
    assert!(tail.next_batch().unwrap().is_empty());
}

#[test]
fn an_empty_newest_file_keeps_its_first_seq() {
    let dir = tempfile::tempdir().unwrap();
    let j = Journal::open(dir.path()).unwrap();
    j.append(&mut [ev(1)]).unwrap();
    // a crash right after a rotation created the file
    std::fs::write(j.dir().join("journal-000000000002.jsonl"), b"").unwrap();
    assert_eq!(j.append(&mut [ev(2)]).unwrap(), vec![2]);
    assert_eq!(j.files().unwrap().len(), 2);
    assert_eq!(seqs(&j), vec![1, 2]);
}

#[test]
fn event_lines_have_the_fixed_field_order() {
    let mut e = Event::new(
        "jordi",
        Origin::Device,
        Some("SN078C10000000".into()),
        "op-a",
        EventBody::FileModified {
            id: 4711,
            path: "NOTE/Note/Work/Bike.note".into(),
            kind: FileKind::Note,
            md5: "0cc175b9c0f1b6a831c399e269772661".into(),
            prev_md5: "92eb5ffee6ae2fec3ad71c777531578f".into(),
            size: 1135077,
            pages_changed: Some(2),
        },
    );
    e.seq = 1842;
    e.ts = 1791270408879;
    assert_eq!(
        e.to_line(),
        r#"{"seq":1842,"ts":1791270408879,"account":"jordi","origin":"device","device":"SN078C10000000","type":"file.modified","id":4711,"path":"NOTE/Note/Work/Bike.note","kind":"note","md5":"0cc175b9c0f1b6a831c399e269772661","prevMd5":"92eb5ffee6ae2fec3ad71c777531578f","size":1135077,"pagesChanged":2,"opId":"op-a"}"#
    );
    let back: Event = serde_json::from_str(&e.to_line()).unwrap();
    assert_eq!(back, e);

    let mut d = Event::new(
        "jordi",
        Origin::Device,
        Some("SN1".into()),
        "op-b",
        EventBody::DeviceBound,
    );
    d.seq = 1;
    d.ts = 2;
    assert_eq!(
        d.to_line(),
        r#"{"seq":1,"ts":2,"account":"jordi","origin":"device","device":"SN1","type":"device.bound","opId":"op-b"}"#
    );
    assert_eq!(serde_json::from_str::<Event>(&d.to_line()).unwrap(), d);

    // Lines written before `snapshot` existed keep parsing, and a snapshot line says so.
    let old = r#"{"seq":5,"ts":6,"account":"jordi","origin":"cli","type":"folder.created","id":2,"path":"INBOX","opId":"op-d"}"#;
    assert_eq!(
        serde_json::from_str::<Event>(old).unwrap().origin,
        Origin::Cli
    );
    let mut s = Event::new(
        "jordi",
        Origin::Snapshot,
        None,
        "op-e",
        EventBody::FolderCreated {
            id: 2,
            path: "INBOX".into(),
        },
    );
    s.seq = 5;
    s.ts = 6;
    assert!(s.to_line().contains(r#""origin":"snapshot""#));

    let c = Event::new(
        "jordi",
        Origin::Repair,
        None,
        "op-c",
        EventBody::FileCreated {
            id: 1,
            path: "a.pdf".into(),
            kind: FileKind::Document,
            md5: "m".into(),
            size: 3,
            pages_changed: None,
        },
    );
    assert_eq!(
        c.to_line(),
        r#"{"seq":0,"ts":0,"account":"jordi","origin":"repair","type":"file.created","id":1,"path":"a.pdf","kind":"document","md5":"m","size":3,"opId":"op-c"}"#
    );
    for (body, text) in [
        (
            EventBody::FileMoved {
                id: 1,
                path: "b".into(),
                old_path: "a".into(),
            },
            r#""type":"file.moved","id":1,"path":"b","oldPath":"a""#,
        ),
        (
            EventBody::FileCopied {
                id: 2,
                source_id: 1,
                path: "c".into(),
            },
            r#""type":"file.copied","id":2,"sourceId":1,"path":"c""#,
        ),
        (
            EventBody::FileDeleted {
                id: 1,
                path: "b".into(),
                recycle: "5/b".into(),
            },
            r#""type":"file.deleted","id":1,"path":"b","recycle":"5/b""#,
        ),
        (
            EventBody::FolderMoved {
                id: 1,
                path: "B".into(),
                old_path: "A".into(),
            },
            r#""type":"folder.moved","id":1,"path":"B","oldPath":"A""#,
        ),
        (
            EventBody::FolderDeleted {
                id: 1,
                path: "B".into(),
            },
            r#""type":"folder.deleted","id":1,"path":"B""#,
        ),
        (EventBody::DeviceUnbound, r#""type":"device.unbound""#),
    ] {
        let line = Event::new("a", Origin::Scan, None, "o", body).to_line();
        assert!(line.contains(text), "{line}");
        assert!(line.contains(r#""origin":"scan""#), "{line}");
    }
}

#[test]
fn file_kind_by_extension() {
    for (name, kind) in [
        ("x.note", FileKind::Note),
        ("X.NOTE", FileKind::Note),
        ("a.pdf", FileKind::Document),
        ("b.epub", FileKind::Document),
        ("c.docx", FileKind::Document),
        ("a.pdf.mark", FileKind::Mark),
        ("s.png", FileKind::Image),
        ("s.JPG", FileKind::Image),
        ("noext", FileKind::Other),
        (".note", FileKind::Other),
        ("z.zip", FileKind::Other),
    ] {
        assert_eq!(FileKind::of(name), kind, "{name}");
    }
}

#[test]
fn op_ids_are_uuid_v4() {
    let a = new_op_id();
    assert_eq!(a.len(), 36);
    assert_eq!(&a[14..15], "4");
    assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"));
    assert_ne!(a, new_op_id());
}

#[test]
fn cursor_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let c = clock(T0);
    let j = journal_with(dir.path(), 1 << 20, c);
    assert_eq!(j.cursor("crystalline").unwrap(), None);
    let set = j.set_cursor("crystalline", 1234).unwrap();
    assert_eq!(set.seq, 1234);
    assert_eq!(set.updated_at, T0 as u64);
    assert_eq!(j.cursor("crystalline").unwrap(), Some(set));
    let text =
        std::fs::read_to_string(dir.path().join("journal/consumers/crystalline.cursor")).unwrap();
    assert_eq!(
        text,
        format!("{{\n  \"seq\": 1234,\n  \"updatedAt\": {T0}\n}}\n")
    );
    j.set_cursor("crystalline", 1235).unwrap();
    assert_eq!(j.cursor("crystalline").unwrap().unwrap().seq, 1235);
    for bad in ["", "../x", "A", "a/b", ".hidden"] {
        assert!(j.set_cursor(bad, 1).is_err(), "{bad:?}");
    }
}

// ---------------------------------------------------------------- commit protocol

struct Fixture {
    dir: tempfile::TempDir,
    store: Store,
    journal: Journal,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        Accounts::new(dir.path())
            .create("jordi", "jordi@example.org", "secret")
            .unwrap();
        let store = Store::open(dir.path(), "jordi").unwrap();
        {
            let g = store.lock().unwrap();
            store.ensure_native_folders(&g).unwrap();
        }
        let journal = journal_with(dir.path(), 1 << 20, clock(T0));
        Fixture {
            dir,
            store,
            journal,
        }
    }

    /// A staged file inside the account folder.
    fn stage(&self, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let staging = self.store.account_dir().join("staging/u1");
        std::fs::create_dir_all(&staging).unwrap();
        let p = staging.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn put(&self, path: &str, bytes: &[u8]) -> Vec<u64> {
        let g = self.store.lock_recovered(&self.journal).unwrap();
        let staged = self.stage("f", bytes);
        let c = self
            .store
            .plan_put(&g, &wp(path), &staged, Origin::Device, Some("SN1".into()))
            .unwrap();
        self.store.commit(&g, c, &self.journal).unwrap()
    }

    fn events(&self) -> Vec<Event> {
        all(&self.journal)
    }

    /// A second process after a crash: fresh handles, then recovery.
    fn restart_and_recover(&self) -> Recovery {
        let store = Store::open(self.dir.path(), "jordi").unwrap();
        let journal = Journal::open(self.dir.path()).unwrap();
        let g = store.lock().unwrap();
        store.recover(&g, &journal).unwrap()
    }

    fn pending_exists(&self) -> bool {
        self.store.account_dir().join(PENDING_FILE).exists()
    }
}

fn md5_hex(bytes: &[u8]) -> String {
    use md5::{Digest, Md5};
    const_hex::encode(Md5::digest(bytes))
}

#[test]
fn commits_journal_every_operation() {
    let f = Fixture::new();
    // create
    let s = f.put("NOTE/Note/a.note", b"one");
    assert_eq!(s, vec![1]);
    let id = f.store.id_of(&wp("NOTE/Note/a.note")).unwrap().unwrap();
    assert_eq!(
        std::fs::read(f.store.fs_path(&wp("NOTE/Note/a.note"))).unwrap(),
        b"one"
    );
    // modify keeps the id and carries prevMd5
    f.put("NOTE/Note/a.note", b"two!");
    // folder, copy, move, delete
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let c = f
        .store
        .plan_create_folder(&g, &wp("NOTE/Note/Work"), Origin::Cli, None)
        .unwrap();
    f.store.commit(&g, c, &f.journal).unwrap();
    let c = f
        .store
        .plan_copy(
            &g,
            &wp("NOTE/Note/a.note"),
            &wp("NOTE/Note/Work/b.note"),
            Origin::Cli,
            None,
        )
        .unwrap();
    f.store.commit(&g, c, &f.journal).unwrap();
    let c = f
        .store
        .plan_move(
            &g,
            &wp("NOTE/Note/Work"),
            &wp("NOTE/Note/Play"),
            Origin::Cli,
            None,
        )
        .unwrap();
    f.store.commit(&g, c, &f.journal).unwrap();
    let c = f
        .store
        .plan_delete(&g, &wp("NOTE/Note/a.note"), 5000, Origin::Cli, None)
        .unwrap();
    f.store.commit(&g, c, &f.journal).unwrap();
    drop(g);
    assert!(!f.pending_exists());

    let ev = f.events();
    let types: Vec<&str> = ev.iter().map(|e| e.body.type_name()).collect();
    assert_eq!(
        types,
        vec![
            "file.created",
            "file.modified",
            "folder.created",
            "file.copied",
            "folder.moved",
            "file.deleted"
        ]
    );
    assert_eq!(
        ev.iter().map(|e| e.seq).collect::<Vec<_>>(),
        (1..=6).collect::<Vec<_>>()
    );
    assert_eq!(ev[0].origin, Origin::Device);
    assert_eq!(ev[0].device.as_deref(), Some("SN1"));
    assert_eq!(ev[0].account, "jordi");
    assert_eq!(
        ev[0].body,
        EventBody::FileCreated {
            id,
            path: "NOTE/Note/a.note".into(),
            kind: FileKind::Note,
            md5: md5_hex(b"one"),
            size: 3,
            pages_changed: None,
        }
    );
    assert_eq!(
        ev[1].body,
        EventBody::FileModified {
            id,
            path: "NOTE/Note/a.note".into(),
            kind: FileKind::Note,
            md5: md5_hex(b"two!"),
            prev_md5: md5_hex(b"one"),
            size: 4,
            pages_changed: None,
        }
    );
    let copy_id = f
        .store
        .id_of(&wp("NOTE/Note/Play/b.note"))
        .unwrap()
        .unwrap();
    assert_eq!(
        ev[3].body,
        EventBody::FileCopied {
            id: copy_id,
            source_id: id,
            path: "NOTE/Note/Work/b.note".into()
        }
    );
    let EventBody::FolderMoved { path, old_path, .. } = &ev[4].body else {
        panic!()
    };
    assert_eq!(
        (path.as_str(), old_path.as_str()),
        ("NOTE/Note/Play", "NOTE/Note/Work")
    );
    assert_eq!(
        ev[5].body,
        EventBody::FileDeleted {
            id,
            path: "NOTE/Note/a.note".into(),
            recycle: "5000/NOTE/Note/a.note".into()
        }
    );
    // every event of a commit has its own opId here (one event per commit)
    let mut ops: Vec<&str> = ev.iter().map(|e| e.op_id.as_str()).collect();
    ops.dedup();
    assert_eq!(ops.len(), 6);
    // the data and the registry
    assert!(f.store.id_of(&wp("NOTE/Note/a.note")).unwrap().is_none());
    assert_eq!(
        std::fs::read(f.store.account_dir().join("recycle/5000/NOTE/Note/a.note")).unwrap(),
        b"two!"
    );
    assert_eq!(
        std::fs::read(f.store.fs_path(&wp("NOTE/Note/Play/b.note"))).unwrap(),
        b"two!"
    );
}

#[test]
fn planning_refuses_bad_operations() {
    let f = Fixture::new();
    f.put("NOTE/Note/a.note", b"x");
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let s = &f.store;
    let staged = f.stage("g", b"y");
    // missing parent, outside staging, existing targets, native folders, own subtree
    assert!(
        s.plan_put(&g, &wp("NOPE/a.note"), &staged, Origin::Cli, None)
            .is_err()
    );
    assert!(
        s.plan_put(&g, &wp("NOTE/Note"), &staged, Origin::Cli, None)
            .is_err()
    );
    let outside = f.dir.path().join("outside");
    std::fs::write(&outside, b"z").unwrap();
    assert!(
        s.plan_put(&g, &wp("NOTE/b.note"), &outside, Origin::Cli, None)
            .is_err()
    );
    assert!(
        s.plan_put(&g, &wp("NOTE/Note/A.note"), &staged, Origin::Cli, None)
            .is_err()
    );
    assert!(
        s.plan_copy(
            &g,
            &wp("NOTE/Note/a.note"),
            &wp("NOTE/Note/a.note"),
            Origin::Cli,
            None
        )
        .is_err()
    );
    assert!(
        s.plan_move(
            &g,
            &wp("NOTE/Note/a.note"),
            &wp("NOTE/Note"),
            Origin::Cli,
            None
        )
        .is_err()
    );
    assert!(
        s.plan_move(&g, &wp("NOTE/Note"), &wp("NOTE/X"), Origin::Cli, None)
            .is_err()
    );
    assert!(
        s.plan_delete(&g, &wp("NOTE"), 1, Origin::Cli, None)
            .is_err()
    );
    assert!(
        s.plan_delete(&g, &wp("NOTE/Note/zz"), 1, Origin::Cli, None)
            .is_err()
    );
    assert!(
        s.plan_create_folder(&g, &wp("NOTE"), Origin::Cli, None)
            .is_err()
    );
    let c = s
        .plan_create_folder(&g, &wp("W"), Origin::Cli, None)
        .unwrap();
    s.commit(&g, c, &f.journal).unwrap();
    assert!(
        s.plan_move(&g, &wp("W"), &wp("W/inner"), Origin::Cli, None)
            .is_err()
    );
    drop(g);
    assert_eq!(f.events().len(), 2);
}

#[test]
fn crash_after_the_rename_before_the_append_is_repaired_exactly_once() {
    let f = Fixture::new();
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let staged = f.stage("f", b"hello");
    let c = f
        .store
        .plan_put(
            &g,
            &wp("NOTE/Note/a.note"),
            &staged,
            Origin::Device,
            Some("SN1".into()),
        )
        .unwrap();
    let op_id = c.op_id.clone();
    let id = match &c.op {
        PendingOp::Put { id, .. } => *id,
        _ => unreachable!(),
    };
    f.store
        .commit_until(&g, c, &f.journal, Some(CrashPoint::BeforeAppend))
        .unwrap();
    drop(g);
    // the file is in place, the event is missing, the intent is on disk
    assert!(f.store.fs_path(&wp("NOTE/Note/a.note")).is_file());
    assert!(f.events().is_empty());
    assert!(f.pending_exists());

    let r = f.restart_and_recover();
    assert_eq!(
        r,
        Recovery::Repaired {
            op_id: op_id.clone(),
            seqs: vec![1]
        }
    );
    assert!(!f.pending_exists());
    let ev = f.events();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].origin, Origin::Repair);
    assert_eq!(ev[0].device.as_deref(), Some("SN1"));
    assert_eq!(ev[0].op_id, op_id);
    assert!(matches!(ev[0].body, EventBody::FileCreated { id: i, .. } if i == id));
    // a second recovery adds nothing
    assert_eq!(f.restart_and_recover(), Recovery::Clean);
    assert_eq!(f.events().len(), 1);
}

#[test]
fn crash_before_the_rename_discards_the_intent() {
    let f = Fixture::new();
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let staged = f.stage("f", b"hello");
    let c = f
        .store
        .plan_put(&g, &wp("NOTE/Note/a.note"), &staged, Origin::Device, None)
        .unwrap();
    let op_id = c.op_id.clone();
    f.store
        .commit_until(&g, c, &f.journal, Some(CrashPoint::BeforeApply))
        .unwrap();
    drop(g);
    assert!(f.pending_exists());
    assert_eq!(f.restart_and_recover(), Recovery::Discarded { op_id });
    assert!(!f.pending_exists());
    assert!(!f.store.fs_path(&wp("NOTE/Note/a.note")).exists());
    assert!(f.events().is_empty());
    assert!(staged.is_file(), "the staged file is not touched");
}

#[test]
fn crash_before_the_rename_of_a_modify_discards_the_intent() {
    let f = Fixture::new();
    f.put("NOTE/Note/a.note", b"old");
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let staged = f.stage("f", b"new");
    let c = f
        .store
        .plan_put(&g, &wp("NOTE/Note/a.note"), &staged, Origin::Device, None)
        .unwrap();
    f.store
        .commit_until(&g, c, &f.journal, Some(CrashPoint::BeforeApply))
        .unwrap();
    drop(g);
    assert!(matches!(
        f.restart_and_recover(),
        Recovery::Discarded { .. }
    ));
    assert_eq!(f.events().len(), 1);
}

#[test]
fn crash_after_the_append_before_clearing_does_not_duplicate() {
    let f = Fixture::new();
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let staged = f.stage("f", b"hello");
    let c = f
        .store
        .plan_put(&g, &wp("NOTE/Note/a.note"), &staged, Origin::Device, None)
        .unwrap();
    let op_id = c.op_id.clone();
    let s = f
        .store
        .commit_until(&g, c, &f.journal, Some(CrashPoint::BeforeClear))
        .unwrap();
    assert_eq!(s, vec![1]);
    drop(g);
    assert!(f.pending_exists());
    assert_eq!(
        f.restart_and_recover(),
        Recovery::Repaired {
            op_id,
            seqs: vec![]
        }
    );
    assert!(!f.pending_exists());
    let ev = f.events();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].origin, Origin::Device);
}

#[test]
fn a_torn_batch_gets_only_its_missing_events() {
    let f = Fixture::new();
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let staged = f.stage("f", b"hello");
    let mut c = f
        .store
        .plan_put(&g, &wp("NOTE/Note/a.note"), &staged, Origin::Device, None)
        .unwrap();
    // further planned events of the same commit (as the page indexer adds them)
    c.push_event(EventBody::FolderCreated {
        id: 1,
        path: "x1".into(),
    })
    .push_event(EventBody::FolderCreated {
        id: 2,
        path: "x2".into(),
    });
    let op_id = c.op_id.clone();
    let planned = c.events.clone();
    f.store
        .commit_until(&g, c, &f.journal, Some(CrashPoint::BeforeAppend))
        .unwrap();
    drop(g);
    // the crash left the first line of the batch and half of the second
    let mut first = Event::new("jordi", Origin::Device, None, &op_id, planned[0].clone());
    f.journal.append(std::slice::from_mut(&mut first)).unwrap();
    let file = f.journal.files().unwrap()[0].1.clone();
    let mut text = std::fs::read_to_string(&file).unwrap();
    text.push_str(r#"{"seq":2,"ts":1,"account":"jordi","#);
    std::fs::write(&file, text).unwrap();

    let r = f.restart_and_recover();
    assert_eq!(
        r,
        Recovery::Repaired {
            op_id: op_id.clone(),
            seqs: vec![2, 3]
        }
    );
    let ev = f.events();
    assert_eq!(ev.len(), 3);
    assert!(ev.iter().all(|e| e.op_id == op_id));
    assert_eq!(
        ev.iter().map(|e| e.body.clone()).collect::<Vec<_>>(),
        planned
    );
    assert_eq!(ev[0].origin, Origin::Device);
    assert_eq!(ev[1].origin, Origin::Repair);
    assert_eq!(f.restart_and_recover(), Recovery::Clean);
    assert_eq!(f.events().len(), 3);
}

#[test]
fn the_repair_scan_reaches_into_the_previous_file() {
    let dir = tempfile::tempdir().unwrap();
    Accounts::new(dir.path())
        .create("jordi", "jordi@example.org", "secret")
        .unwrap();
    let store = Store::open(dir.path(), "jordi").unwrap();
    // a cap of one byte: every append after a non-empty file starts a new one
    let journal = journal_with(dir.path(), 1, clock(T0));
    let g = store.lock_recovered(&journal).unwrap();
    store.ensure_native_folders(&g).unwrap();
    let c = store
        .plan_create_folder(&g, &wp("A"), Origin::Cli, None)
        .unwrap();
    store
        .commit_until(&g, c, &journal, Some(CrashPoint::BeforeClear))
        .unwrap();
    // other accounts kept appending: the commit's line is now in an older file
    for i in 0..3 {
        journal.append(&mut [ev(i)]).unwrap();
    }
    assert!(journal.files().unwrap().len() >= 2);
    let r = store.recover(&g, &journal).unwrap();
    assert!(matches!(r, Recovery::Repaired { seqs, .. } if seqs.is_empty()));
    assert_eq!(journal.read_from(0).unwrap().count(), 4);
}

#[test]
fn a_crashed_move_is_repaired_with_the_registry() {
    let f = Fixture::new();
    f.put("NOTE/Note/a.note", b"x");
    let id = f.store.id_of(&wp("NOTE/Note/a.note")).unwrap().unwrap();
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let c = f
        .store
        .plan_move(
            &g,
            &wp("NOTE/Note/a.note"),
            &wp("NOTE/b.note"),
            Origin::Device,
            None,
        )
        .unwrap();
    f.store
        .commit_until(&g, c, &f.journal, Some(CrashPoint::BeforeAppend))
        .unwrap();
    drop(g);
    // the rename happened, the registry still has the old path
    assert_eq!(f.store.path_of(id).unwrap(), Some(wp("NOTE/Note/a.note")));
    assert!(matches!(f.restart_and_recover(), Recovery::Repaired { .. }));
    assert_eq!(f.store.path_of(id).unwrap(), Some(wp("NOTE/b.note")));
    let ev = f.events();
    assert_eq!(
        ev[1].body,
        EventBody::FileMoved {
            id,
            path: "NOTE/b.note".into(),
            old_path: "NOTE/Note/a.note".into()
        }
    );
    // the listing shows the file under its old id
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let e = f.store.entry(&g, &wp("NOTE/b.note")).unwrap().unwrap();
    assert_eq!(e.id, id);
}

#[test]
fn a_crashed_delete_is_repaired_and_a_crashed_folder_create_too() {
    let f = Fixture::new();
    f.put("NOTE/Note/a.note", b"x");
    let id = f.store.id_of(&wp("NOTE/Note/a.note")).unwrap().unwrap();
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let c = f
        .store
        .plan_delete(&g, &wp("NOTE/Note/a.note"), 77, Origin::Device, None)
        .unwrap();
    f.store
        .commit_until(&g, c, &f.journal, Some(CrashPoint::BeforeAppend))
        .unwrap();
    drop(g);
    assert!(matches!(f.restart_and_recover(), Recovery::Repaired { .. }));
    assert_eq!(f.store.path_of(id).unwrap(), None);
    assert!(
        f.store
            .account_dir()
            .join("recycle/77/NOTE/Note/a.note")
            .is_file()
    );

    let g = f.store.lock_recovered(&f.journal).unwrap();
    let c = f
        .store
        .plan_create_folder(&g, &wp("INBOX/New"), Origin::Device, None)
        .unwrap();
    f.store
        .commit_until(&g, c, &f.journal, Some(CrashPoint::BeforeAppend))
        .unwrap();
    drop(g);
    assert!(matches!(f.restart_and_recover(), Recovery::Repaired { .. }));
    let types: Vec<&str> = f.events().iter().map(|e| e.body.type_name()).collect();
    assert_eq!(
        types,
        vec!["file.created", "file.deleted", "folder.created"]
    );
}

#[test]
fn a_commit_first_finishes_an_earlier_unfinished_one() {
    let f = Fixture::new();
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let staged = f.stage("f", b"one");
    let c = f
        .store
        .plan_put(&g, &wp("NOTE/Note/a.note"), &staged, Origin::Cli, None)
        .unwrap();
    f.store
        .commit_until(&g, c, &f.journal, Some(CrashPoint::BeforeAppend))
        .unwrap();
    drop(g);
    // the long-running server commits next, without a restart
    f.put("NOTE/Note/b.note", b"two");
    let ev = f.events();
    assert_eq!(ev.len(), 2);
    assert_eq!(ev[0].origin, Origin::Repair);
    assert!(
        matches!(&ev[0].body, EventBody::FileCreated { path, .. } if path == "NOTE/Note/a.note")
    );
    assert!(
        matches!(&ev[1].body, EventBody::FileCreated { path, .. } if path == "NOTE/Note/b.note")
    );
    assert!(!f.pending_exists());
}

#[test]
fn the_server_sweeps_stale_temp_files_at_start_and_hourly_not_recovery() {
    let f = Fixture::new();
    let dir = f.store.fs_path(&wp("NOTE/Note"));
    let old = dir.join(".nd-1-1.tmp");
    let fresh = dir.join(".nd-1-2.tmp");
    let other = dir.join(".keep.tmp");
    for p in [&old, &fresh, &other] {
        std::fs::write(p, b"x").unwrap();
    }
    let two_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
    for p in [&old, &other] {
        std::fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(two_hours_ago)
            .unwrap();
    }
    // Recovery (every lock of every request) does not walk the tree any more.
    assert_eq!(f.restart_and_recover(), Recovery::Clean);
    assert!(old.exists());
    // The server start sweeps.
    let state =
        notedown_cloud::server::AppState::open(notedown_cloud::Config::default(), f.dir.path())
            .unwrap();
    assert!(!old.exists());
    assert!(fresh.exists());
    assert!(other.exists());
    // And the hourly sweep does too.
    std::fs::write(&old, b"x").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(two_hours_ago)
        .unwrap();
    let g = f.store.lock_recovered(&f.journal).unwrap();
    drop(g);
    assert!(old.exists());
    assert_eq!(state.sweep_temps().unwrap(), 1);
    assert!(!old.exists());
    assert!(fresh.exists());
}

#[test]
fn recover_all_leaves_accounts_without_files_alone() {
    let dir = tempfile::tempdir().unwrap();
    let accounts = Accounts::new(dir.path());
    accounts
        .create("idle", "idle@example.org", "secret")
        .unwrap();
    accounts
        .create("busy", "busy@example.org", "secret")
        .unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    {
        let store = Store::open(dir.path(), "busy").unwrap();
        let g = store.lock_recovered(&journal).unwrap();
        store.ensure_native_folders(&g).unwrap();
        let c = store
            .plan_create_folder(&g, &wp("A"), Origin::Cli, None)
            .unwrap();
        store
            .commit_until(&g, c, &journal, Some(CrashPoint::BeforeAppend))
            .unwrap();
    }
    let done = recover_all(dir.path(), &journal).unwrap();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].0, "busy");
    assert!(matches!(done[0].1, Ok(Recovery::Repaired { .. })));
    assert!(!accounts.account_dir("idle").join("files").exists());
    assert_eq!(journal.read_from(0).unwrap().count(), 1);
    assert!(recover_all(dir.path(), &journal).unwrap().is_empty());
}

#[test]
fn snapshot_journals_the_current_state() {
    let f = Fixture::new();
    f.put("NOTE/Note/a.note", b"one");
    f.put("INBOX/b.pdf", b"two");
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let before = f.events().len();
    let s = f.store.snapshot(&g, &f.journal).unwrap();
    let ev: Vec<Event> = f.events().into_iter().skip(before).collect();
    assert_eq!(s.len(), ev.len());
    // 8 native folders, 2 files and page.index_failed for a.note ("one" is not a note)
    assert_eq!(ev.len(), 11);
    let failed = ev
        .iter()
        .position(|e| e.body.type_name() == "page.index_failed")
        .unwrap();
    assert!(
        matches!(&ev[failed - 1].body, EventBody::FileCreated { path, .. } if path == "NOTE/Note/a.note")
    );
    assert!(
        ev.iter()
            .all(|e| e.origin == Origin::Snapshot && e.op_id == ev[0].op_id)
    );
    let a = ev
        .iter()
        .find_map(|e| match &e.body {
            EventBody::FileCreated {
                path,
                kind,
                md5,
                size,
                ..
            } if path == "NOTE/Note/a.note" => Some((*kind, md5.clone(), *size)),
            _ => None,
        })
        .unwrap();
    assert_eq!(a, (FileKind::Note, md5_hex(b"one"), 3));
    let note = ev
        .iter()
        .position(
            |e| matches!(&e.body, EventBody::FolderCreated { path, .. } if path == "NOTE/Note"),
        )
        .unwrap();
    let file = ev.iter().position(|e| matches!(&e.body, EventBody::FileCreated { path, .. } if path == "NOTE/Note/a.note")).unwrap();
    assert!(note < file, "parents first");
}

// ---------------------------------------------------------------- fix round 1

#[test]
fn a_fresh_handle_plans_only_on_the_recovered_registry() {
    let f = Fixture::new();
    f.put("NOTE/Note/a.note", b"x");
    let id = f.store.id_of(&wp("NOTE/Note/a.note")).unwrap().unwrap();
    {
        let g = f.store.lock_recovered(&f.journal).unwrap();
        let c = f
            .store
            .plan_move(
                &g,
                &wp("NOTE/Note/a.note"),
                &wp("NOTE/b.note"),
                Origin::Cli,
                None,
            )
            .unwrap();
        f.store
            .commit_until(&g, c, &f.journal, Some(CrashPoint::BeforeAppend))
            .unwrap();
        // under the same guard nothing can be planned on top of the unfinished change
        let err = f
            .store
            .plan_create_folder(&g, &wp("INBOX/X"), Origin::Cli, None)
            .unwrap_err();
        assert!(err.to_string().contains("unfinished change"), "{err}");
    }
    // the server, a long-running handle of its own, deletes the moved file next
    let store = Store::open(f.dir.path(), "jordi").unwrap();
    let journal = Journal::open(f.dir.path()).unwrap();
    let g = store.lock_recovered(&journal).unwrap();
    let c = store
        .plan_delete(&g, &wp("NOTE/b.note"), 9, Origin::Device, None)
        .unwrap();
    store.commit(&g, c, &journal).unwrap();
    drop(g);
    let ev = f.events();
    let types: Vec<&str> = ev.iter().map(|e| e.body.type_name()).collect();
    assert_eq!(types, ["file.created", "file.moved", "file.deleted"]);
    assert_eq!(ev[1].origin, Origin::Repair);
    assert!(matches!(ev[2].body, EventBody::FileDeleted { id: d, .. } if d == id));
    // no stale row: neither the old nor the new path nor the id is registered
    assert_eq!(store.path_of(id).unwrap(), None);
    assert_eq!(store.id_of(&wp("NOTE/Note/a.note")).unwrap(), None);
    assert_eq!(store.id_of(&wp("NOTE/b.note")).unwrap(), None);
}

#[test]
fn the_repair_finds_its_events_after_more_than_a_thousand_other_lines() {
    let f = Fixture::new();
    f.put("NOTE/Note/z.note", b"z"); // the journal is not empty at the commit start
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let staged = f.stage("f", b"hello");
    let c = f
        .store
        .plan_put(&g, &wp("NOTE/Note/a.note"), &staged, Origin::Device, None)
        .unwrap();
    let op_id = c.op_id.clone();
    f.store
        .commit_until(&g, c, &f.journal, Some(CrashPoint::BeforeClear))
        .unwrap();
    drop(g);
    let pending: serde_json::Value =
        serde_json::from_slice(&std::fs::read(f.store.account_dir().join(PENDING_FILE)).unwrap())
            .unwrap();
    assert_eq!(pending["journalSeq"], 1);
    // other accounts append 1100 lines before the restart
    let other = Journal::open(f.dir.path()).unwrap();
    let mut batch: Vec<Event> = (0..1100).map(ev).collect();
    other.append(&mut batch).unwrap();
    assert_eq!(
        f.restart_and_recover(),
        Recovery::Repaired {
            op_id: op_id.clone(),
            seqs: vec![]
        }
    );
    assert_eq!(f.events().iter().filter(|e| e.op_id == op_id).count(), 1);
    assert_eq!(f.events().len(), 1102);
}

#[test]
fn a_put_of_identical_bytes_is_a_no_op() {
    let f = Fixture::new();
    f.put("NOTE/Note/a.note", b"same");
    let target = f.store.fs_path(&wp("NOTE/Note/a.note"));
    let ino = |p: &Path| {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(p).unwrap().ino()
    };
    let before = ino(&target);
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let staged = f.stage("f", b"same");
    let c = f
        .store
        .plan_put(&g, &wp("NOTE/Note/a.note"), &staged, Origin::Device, None)
        .unwrap();
    assert!(matches!(c.op, PendingOp::Unchanged { .. }));
    assert!(c.events.is_empty());
    assert_eq!(
        f.store.commit(&g, c, &f.journal).unwrap(),
        Vec::<u64>::new()
    );
    drop(g);
    assert_eq!(f.events().len(), 1);
    assert_eq!(ino(&target), before, "no data write");
    assert!(staged.is_file(), "the staged file is left to the caller");
    assert!(!f.pending_exists());
}

#[test]
fn a_broken_intent_stops_only_its_own_account() {
    let dir = tempfile::tempdir().unwrap();
    let accounts = Accounts::new(dir.path());
    accounts
        .create("broken", "b@example.org", "secret")
        .unwrap();
    accounts.create("good", "g@example.org", "secret").unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    std::fs::create_dir_all(accounts.account_dir("broken").join("files")).unwrap();
    std::fs::write(
        accounts.account_dir("broken").join(PENDING_FILE),
        b"{not json",
    )
    .unwrap();
    {
        let store = Store::open(dir.path(), "good").unwrap();
        let g = store.lock_recovered(&journal).unwrap();
        store.ensure_native_folders(&g).unwrap();
        let c = store
            .plan_create_folder(&g, &wp("A"), Origin::Cli, None)
            .unwrap();
        store
            .commit_until(&g, c, &journal, Some(CrashPoint::BeforeAppend))
            .unwrap();
    }
    let done = recover_all(dir.path(), &journal).unwrap();
    assert_eq!(done.len(), 2);
    assert_eq!(done[0].0, "broken");
    assert!(done[0].1.is_err());
    assert_eq!(done[1].0, "good");
    assert!(matches!(done[1].1, Ok(Recovery::Repaired { .. })));
    // a write to the broken account fails with the reason
    let store = Store::open(dir.path(), "broken").unwrap();
    let err = store.lock_recovered(&journal).unwrap_err().to_string();
    assert!(err.contains("pending.json"), "{err}");
    assert!(
        err.contains("blocks every write to account broken"),
        "{err}"
    );
}

#[test]
fn a_rotation_cuts_the_torn_tail_of_the_old_file() {
    let dir = tempfile::tempdir().unwrap();
    // 2026-10-31T23:59:59.000Z
    let c = clock(1_793_491_199_000);
    let j = journal_with(dir.path(), 1 << 30, c.clone());
    j.append(&mut [ev(1)]).unwrap();
    let old = j.files().unwrap()[0].1.clone();
    let mut text = std::fs::read_to_string(&old).unwrap();
    text.push_str("{\"seq\":2,\"ts\":1");
    std::fs::write(&old, text).unwrap();
    c.0.fetch_add(5_000, Ordering::SeqCst); // November
    assert_eq!(j.append(&mut [ev(2)]).unwrap(), vec![2]);
    assert_eq!(j.files().unwrap().len(), 2);
    let text = std::fs::read_to_string(&old).unwrap();
    assert!(text.ends_with('\n'), "{text}");
    assert_eq!(text.lines().count(), 1);
    assert_eq!(seqs(&j), vec![1, 2]);
}

#[test]
fn recovery_messages_count_events_in_plain_english() {
    let one = Recovery::Repaired {
        op_id: "o".into(),
        seqs: vec![7],
    };
    assert_eq!(
        one.to_string(),
        "finished the change o and journaled 1 missing event"
    );
    let two = Recovery::Repaired {
        op_id: "o".into(),
        seqs: vec![7, 8],
    };
    assert_eq!(
        two.to_string(),
        "finished the change o and journaled 2 missing events"
    );
}
