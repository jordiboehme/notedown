use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const ACCOUNT: &str = "jordi";

/// A data folder and a config folder of their own, never the user's.
struct Fixture {
    data: tempfile::TempDir,
    config: tempfile::TempDir,
    account: Arc<Mutex<Option<String>>>,
    data_dir: Arc<Mutex<PathBuf>>,
    events: Arc<AtomicUsize>,
    notes: Notes,
}

impl Fixture {
    fn new() -> Self {
        Self::with_seen(None)
    }

    /// With `seen` as the content of seen.json before the first scan.
    fn with_seen(seen: Option<Value>) -> Self {
        let data = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        if let Some(seen) = seen {
            fs::write(config.path().join("seen.json"), seen.to_string()).unwrap();
        }
        fs::create_dir_all(data.path().join("pipeline").join(ACCOUNT).join("notes")).unwrap();
        let account = Arc::new(Mutex::new(Some(ACCOUNT.to_owned())));
        let data_dir = Arc::new(Mutex::new(data.path().to_path_buf()));
        let events = Arc::new(AtomicUsize::new(0));
        let notes = Notes::new(
            {
                let account = account.clone();
                let data_dir = data_dir.clone();
                Arc::new(move || (lock(&data_dir).clone(), lock(&account).clone()))
            },
            config.path().to_path_buf(),
            {
                let events = events.clone();
                Arc::new(move || {
                    events.fetch_add(1, Ordering::SeqCst);
                })
            },
        );
        Self {
            data,
            config,
            account,
            data_dir,
            events,
            notes,
        }
    }

    fn notes_dir(&self) -> PathBuf {
        lock(&self.data_dir)
            .join("pipeline")
            .join(lock(&self.account).clone().unwrap())
            .join("notes")
    }

    fn file_of(&self, key: &str) -> PathBuf {
        let name = match key.strip_prefix("path:") {
            Some(md5) => format!("path-{md5}.json"),
            None => format!("{key}.json"),
        };
        self.notes_dir().join(name)
    }

    /// Writes the state of a note as the pipeline does (a temp file renamed over the old one),
    /// with the mtime `secs` seconds after a fixed time.
    fn write(&self, note: &Value, secs: u64) {
        let key = note["key"].as_str().unwrap();
        let path = self.file_of(key);
        let temp = self.notes_dir().join(format!(".{key}.json.1.1.tmp"));
        fs::write(&temp, serde_json::to_vec_pretty(note).unwrap()).unwrap();
        fs::rename(&temp, &path).unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(at(secs))
            .unwrap();
    }

    fn remove(&self, key: &str) {
        fs::remove_file(self.file_of(key)).unwrap();
    }

    fn seen_file(&self) -> Value {
        serde_json::from_slice(&fs::read(self.config.path().join("seen.json")).unwrap()).unwrap()
    }

    /// The markers of the account in the current data folder, `null` when it has none.
    fn marks(&self) -> Value {
        let dir = lock(&self.data_dir).clone();
        marks_in(&self.seen_file(), &dir)
    }

    fn new_keys(&self) -> Vec<String> {
        self.notes.new_notes().into_iter().map(|n| n.key).collect()
    }

    fn all_keys(&self) -> Vec<String> {
        self.notes.all().into_iter().map(|n| n.key).collect()
    }

    fn events(&self) -> usize {
        self.events.load(Ordering::SeqCst)
    }
}

/// The markers of the account in `dir` in a seen.json, `null` when it has none.
fn marks_in(seen: &Value, dir: &Path) -> Value {
    seen[ACCOUNT]
        .as_array()
        .into_iter()
        .flatten()
        .find(|folder| folder["dataDir"] == json!(dir))
        .map_or(Value::Null, |folder| folder["seen"].clone())
}

fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs)
}

fn ms(secs: u64) -> i64 {
    i64::try_from(at(secs).duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap()
}

/// A page: id, the seq of its text version, and whether that text has blocks.
type Page<'a> = (&'a str, Option<u64>, bool);

/// The state of a note as the pipeline writes it (crates/notedown-pipeline/src/model.rs),
/// strokes and the device's recognition text included.
fn note(key: &str, path: &str, pages: &[Page<'_>]) -> Value {
    let pages: Vec<Value> = pages
        .iter()
        .enumerate()
        .map(|(position, (id, seq, text))| {
            let state = match (seq, text) {
                (Some(_), _) => "done",
                (None, _) => "pending",
            };
            let blocks = if *text {
                json!([{"id": "b1", "label": "text", "bbox": [1, 2, 3, 4], "text": "Hello"}])
            } else {
                json!([])
            };
            json!({
                "pageId": id,
                "position": position,
                "pageHash": "ph",
                "strokesHash": "sh",
                "layersHash": "lh",
                "seq": seq,
                "state": state,
                "strokeList": [{"x": 1, "y": 2}, {"x": 3, "y": 4}],
                "blocks": blocks,
            })
        })
        .collect();
    let title = path
        .rsplit('/')
        .next()
        .unwrap()
        .trim_end_matches(".note")
        .to_owned();
    json!({
        "key": key,
        "path": path,
        "title": title,
        "size": 1234,
        "md5": "0123456789abcdef0123456789abcdef",
        "model": "chandra",
        "meta": {
            "keywords": ["k"],
            "titles": [],
            "links": [],
            "stars": [],
            "recognition": {"P1": "device text"},
            "digests": [],
        },
        "pages": pages,
        "deleted": false,
        "successors": {},
        "nextBlock": 2,
        "logSeq": 1,
    })
}

const A: &str = "F20240708174639550429bHksqaP2wKQ8";
const B: &str = "F20240708174639550429bHksqaP2wKQ9";
const C: &str = "F20240708174639550429bHksqaP2wKQ7";

#[test]
fn the_index_leaves_out_strokes() {
    let f = Fixture::new();
    let mut a = note(
        A,
        "NOTE/Note/a.note",
        &[("P1", Some(4), true), ("P2", None, false)],
    );
    // Fields the index does not read are not parsed into anything: not even a wrong type
    // there keeps the note out.
    a["pages"][0]["strokeList"] = json!("not a list");
    a["meta"]["recognition"] = json!(42);
    f.write(&a, 10);
    let all = f.notes.all();
    assert_eq!(
        all,
        vec![NoteSummary {
            key: A.into(),
            path: "NOTE/Note/a.note".into(),
            title: "a".into(),
            pages: 2,
            pages_with_text: 1,
            changed_ms: ms(10),
        }]
    );
    let sent = serde_json::to_value(&all[0]).unwrap();
    assert_eq!(
        sent,
        json!({"key": A, "path": "NOTE/Note/a.note", "title": "a", "pages": 2,
               "pagesWithText": 1, "changedMs": ms(10)})
    );
}

#[test]
fn the_index_follows_a_changed_store_file() {
    let f = Fixture::new();
    f.write(
        &note(A, "NOTE/Note/aaaa.note", &[("P1", Some(4), true)]),
        10,
    );
    assert_eq!(f.notes.all()[0].title, "aaaa");
    // The same content length and mtime, written in place (no rename): not read again.
    let path = f.file_of(A);
    let text = fs::read_to_string(&path).unwrap().replace("aaaa", "bbbb");
    fs::write(&path, text).unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(at(10))
        .unwrap();
    assert_eq!(f.notes.all()[0].title, "aaaa");
    // A new mtime: read again.
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(at(20))
        .unwrap();
    let all = f.notes.all();
    assert_eq!(all[0].title, "bbbb");
    assert_eq!(all[0].changed_ms, ms(20));
    // A rename by the pipeline at the same mtime: read again (a new inode).
    f.write(
        &note(
            A,
            "NOTE/Note/cccc.note",
            &[("P1", Some(4), true), ("P2", Some(5), true)],
        ),
        20,
    );
    let all = f.notes.all();
    assert_eq!((all[0].title.as_str(), all[0].pages), ("cccc", 2));
    // A file that goes leaves the index; temp files and other names never enter it.
    fs::write(f.notes_dir().join(".x.json.1.2.tmp"), "{").unwrap();
    fs::write(f.notes_dir().join("notes.txt"), "x").unwrap();
    f.remove(A);
    assert!(f.notes.all().is_empty());
}

#[test]
fn a_live_state_wins_over_a_deleted_one_at_the_same_path() {
    let f = Fixture::new();
    let mut old = note(A, "NOTE/Note/a.note", &[("P1", Some(4), true)]);
    old["deleted"] = json!(true);
    // The deleted state is the newer file, and still loses.
    f.write(&old, 30);
    f.write(&note(B, "NOTE/Note/a.note", &[("P1", Some(9), true)]), 20);
    assert_eq!(f.all_keys(), [B]);
    assert_eq!(f.notes.at_path("NOTE/Note/a.note").unwrap().key, B);
    // Unscoped loses too.
    let mut gone = note(B, "NOTE/Note/a.note", &[("P1", Some(9), true)]);
    gone["unscoped"] = json!(true);
    f.write(&gone, 40);
    f.write(&note(C, "NOTE/Note/a.note", &[("P1", Some(10), true)]), 15);
    assert_eq!(f.all_keys(), [C]);
    assert_eq!(f.notes.at_path("NOTE/Note/a.note").unwrap().key, C);
    // With no live state, the path still names its newest state.
    f.remove(C);
    f.notes.refresh();
    assert_eq!(f.notes.at_path("NOTE/Note/a.note").unwrap().key, B);
}

#[test]
fn all_notes_lists_notes_with_text() {
    let f = Fixture::new();
    f.write(&note(A, "NOTE/Note/a.note", &[("P1", Some(4), true)]), 10);
    f.write(&note(B, "NOTE/Note/b.note", &[("P1", Some(5), true)]), 30);
    // Not read yet: pending pages only, or a page read without blocks.
    f.write(&note(C, "NOTE/Note/c.note", &[("P1", None, false)]), 40);
    f.write(
        &note("F4", "NOTE/Note/d.note", &[("P1", Some(6), false)]),
        50,
    );
    let mut deleted = note("F5", "NOTE/Note/e.note", &[("P1", Some(7), true)]);
    deleted["deleted"] = json!(true);
    f.write(&deleted, 60);
    let mut unscoped = note("F6", "NOTE/Note/f.note", &[("P1", Some(8), true)]);
    unscoped["unscoped"] = json!(true);
    f.write(&unscoped, 70);
    // A file that does not parse.
    fs::write(f.notes_dir().join("F7.json"), "{").unwrap();
    // Newest first.
    assert_eq!(f.all_keys(), [B, A]);
    // Without an account there are no notes.
    *lock(&f.account) = None;
    assert!(f.notes.all().is_empty());
}

/// A fixture whose baseline is set (no notes yet), so every note with text is new.
fn after_baseline() -> Fixture {
    let f = Fixture::new();
    assert!(f.new_keys().is_empty());
    assert_eq!(f.marks(), json!({}));
    f
}

#[test]
fn a_note_is_new_until_opened() {
    let f = after_baseline();
    f.write(&note(A, "NOTE/Note/a.note", &[("P1", Some(4), true)]), 10);
    f.write(&note(B, "NOTE/Note/b.note", &[("P1", None, false)]), 11);
    assert_eq!(f.new_keys(), [A]);
    f.notes.seen(A).unwrap();
    assert!(f.new_keys().is_empty());
    assert_eq!(f.marks(), json!({A: 4}));
    // A new page with text: new again.
    f.write(
        &note(
            A,
            "NOTE/Note/a.note",
            &[("P1", Some(4), true), ("P2", Some(12), true)],
        ),
        20,
    );
    assert_eq!(f.new_keys(), [A]);
    f.notes.seen(A).unwrap();
    // The markers outlive the app.
    let again = Notes::new(
        f.notes.inner.place.clone(),
        f.config.path().to_path_buf(),
        Arc::new(|| {}),
    );
    assert!(again.new_notes().is_empty());
    f.write(&note(A, "NOTE/Note/a.note", &[("P1", Some(13), true)]), 30);
    assert_eq!(
        again
            .new_notes()
            .into_iter()
            .map(|n| n.key)
            .collect::<Vec<_>>(),
        [A]
    );
    assert_eq!(f.notes.seen("F404"), Err(NOTE_GONE.to_owned()));
}

#[test]
fn a_save_without_new_text_is_not_new() {
    let f = after_baseline();
    f.write(
        &note(
            A,
            "NOTE/Note/a.note",
            &[("P1", Some(4), true), ("P2", None, false)],
        ),
        10,
    );
    f.notes.seen(A).unwrap();
    // The device saved again (a keyword, a template, a failed OCR try): a new md5, size and
    // logSeq, a new file, the same text versions.
    let mut saved = note(
        A,
        "NOTE/Note/a.note",
        &[("P1", Some(4), true), ("P2", None, false)],
    );
    saved["md5"] = json!("fedcba9876543210fedcba9876543210");
    saved["size"] = json!(9999);
    saved["logSeq"] = json!(150);
    saved["meta"]["keywords"] = json!(["k", "new"]);
    saved["pages"][1]["state"] = json!("failed");
    f.write(&saved, 20);
    assert_eq!(f.notes.all()[0].changed_ms, ms(20));
    assert!(f.new_keys().is_empty());
}

#[test]
fn a_renamed_note_is_not_new() {
    let f = after_baseline();
    f.write(&note(A, "NOTE/Note/a.note", &[("P1", Some(4), true)]), 10);
    f.notes.seen(A).unwrap();
    let mut moved = note(A, "NOTE/Note/projects/b.note", &[("P1", Some(4), true)]);
    moved["logSeq"] = json!(40);
    f.write(&moved, 20);
    let all = f.notes.all();
    assert_eq!(
        (all[0].path.as_str(), all[0].title.as_str()),
        ("NOTE/Note/projects/b.note", "b")
    );
    assert!(f.new_keys().is_empty());
}

#[test]
fn a_deleted_or_unscoped_note_is_not_new() {
    let f = after_baseline();
    f.write(&note(A, "NOTE/Note/a.note", &[("P1", Some(4), true)]), 10);
    f.write(&note(B, "NOTE/Note/b.note", &[("P1", Some(5), true)]), 10);
    assert_eq!(f.new_keys(), [A, B]);
    let mut deleted = note(A, "NOTE/Note/a.note", &[("P1", Some(4), true)]);
    deleted["deleted"] = json!(true);
    f.write(&deleted, 20);
    let mut unscoped = note(B, "NOTE/Note/b.note", &[("P1", Some(5), true)]);
    unscoped["unscoped"] = json!(true);
    f.write(&unscoped, 20);
    assert!(f.new_keys().is_empty());
    // Their state files are still there: their markers stay.
    f.notes.seen(A).unwrap();
    assert_eq!(f.marks(), json!({A: 4}));
}

#[test]
fn the_first_login_sets_the_baseline() {
    // Another account's markers are left alone.
    let other = json!({"other": [{"dataDir": "/x", "seen": {"X": 3}}]});
    let f = Fixture::with_seen(Some(other.clone()));
    f.write(
        &note(
            A,
            "NOTE/Note/a.note",
            &[("P1", Some(4), true), ("P2", None, false)],
        ),
        10,
    );
    f.write(&note(B, "NOTE/Note/b.note", &[("P1", None, false)]), 10);
    let mut deleted = note(C, "NOTE/Note/c.note", &[("P1", Some(2), true)]);
    deleted["deleted"] = json!(true);
    f.write(&deleted, 10);
    assert!(f.new_keys().is_empty());
    assert_eq!(f.marks(), json!({A: 4, C: 2}));
    assert_eq!(f.seen_file()["other"], other["other"]);
    // Notes still in the backlog become new when their text arrives.
    f.write(
        &note(
            A,
            "NOTE/Note/a.note",
            &[("P1", Some(4), true), ("P2", Some(8), true)],
        ),
        20,
    );
    f.write(&note(B, "NOTE/Note/b.note", &[("P1", Some(9), true)]), 20);
    assert_eq!(f.new_keys(), [A, B]);

    // A new account whose store the pipeline has made, still without notes: the baseline is
    // empty, so every note becomes new when its text arrives.
    let g = Fixture::new();
    fs::remove_dir_all(g.notes_dir()).unwrap();
    assert!(g.new_keys().is_empty());
    assert_eq!(g.marks(), json!({}));
    fs::create_dir_all(g.notes_dir()).unwrap();
    g.write(&note(A, "NOTE/Note/a.note", &[("P1", Some(4), true)]), 10);
    assert_eq!(g.new_keys(), [A]);

    // A data folder that is not there (a disk that is not mounted), or one without a store of
    // the account, sets no baseline.
    let h = Fixture::new();
    let away = h.data.path().join("away");
    *lock(&h.data_dir) = away.clone();
    assert!(h.new_keys().is_empty());
    assert!(!h.config.path().join("seen.json").exists());
    fs::create_dir_all(away.join("pipeline").join("someone")).unwrap();
    assert!(h.new_keys().is_empty());
    assert!(!h.config.path().join("seen.json").exists());
    fs::create_dir_all(away.join("pipeline").join(ACCOUNT).join("notes")).unwrap();
    h.write(&note(A, "NOTE/Note/a.note", &[("P1", Some(4), true)]), 10);
    assert!(h.new_keys().is_empty());
    assert_eq!(h.marks(), json!({A: 4}));
}

#[test]
fn a_path_keyed_note_keeps_its_marker_on_a_move() {
    const OLD: &str = "path:0123456789abcdef0123456789abcdef";
    const NEW: &str = "path:fedcba9876543210fedcba9876543210";
    const NEXT: &str = "path:00000000000000000000000000000001";
    let f = after_baseline();
    let pages: &[Page<'_>] = &[("P1", Some(4), true), ("P2", Some(6), true)];
    f.write(&note(OLD, "NOTE/Note/a.note", pages), 10);
    f.notes.seen(OLD).unwrap();
    // The pipeline moves the state to the new key and removes the old file.
    f.write(&note(NEW, "NOTE/Note/b/a.note", pages), 20);
    f.remove(OLD);
    assert!(f.new_keys().is_empty());
    assert_eq!(f.marks(), json!({NEW: 6}));
    // A scan between the two writes sees both; the next one carries the marker over.
    f.write(&note(NEXT, "NOTE/Note/c/a.note", pages), 30);
    assert!(
        f.new_keys().is_empty(),
        "not new while both files are there"
    );
    assert_eq!(f.marks(), json!({NEW: 6, NEXT: 6}));
    f.remove(NEW);
    assert!(f.new_keys().is_empty());
    assert_eq!(f.marks(), json!({NEXT: 6}));
    // A move while the app did not run: the seq of its newest text names the note.
    let later = Notes::new(
        f.notes.inner.place.clone(),
        f.config.path().to_path_buf(),
        Arc::new(|| {}),
    );
    f.write(&note(OLD, "NOTE/Note/a.note", pages), 40);
    f.remove(NEXT);
    assert!(later.new_notes().is_empty());
    assert_eq!(f.marks(), json!({OLD: 6}));
    // A note keyed by its FILE_ID keeps its key; a new one is not given a marker.
    f.write(&note(A, "NOTE/Note/x.note", &[("P9", Some(20), true)]), 50);
    assert_eq!(
        later
            .new_notes()
            .into_iter()
            .map(|n| n.key)
            .collect::<Vec<_>>(),
        [A]
    );
}

#[test]
fn seen_keys_of_gone_notes_are_pruned() {
    let f = after_baseline();
    f.write(&note(A, "NOTE/Note/a.note", &[("P1", Some(4), true)]), 10);
    f.write(&note(B, "NOTE/Note/b.note", &[("P1", Some(5), true)]), 10);
    f.notes.seen(A).unwrap();
    f.notes.seen(B).unwrap();
    assert_eq!(f.marks(), json!({A: 4, B: 5}));
    f.remove(A);
    // A file that does not parse keeps its marker: its key is still there.
    fs::write(f.file_of(B), "{").unwrap();
    f.notes.refresh();
    assert_eq!(f.marks(), json!({B: 5}));
    // A store that cannot be read (a data folder on a disk that is not there) prunes nothing.
    fs::remove_dir_all(f.notes_dir()).unwrap();
    f.notes.refresh();
    assert_eq!(f.marks(), json!({B: 5}));
}

#[test]
fn notes_event_when_the_index_changes() {
    let f = after_baseline();
    let base = f.events();
    assert_eq!(base, 1, "the first scan tells the windows once");
    f.notes.refresh();
    assert_eq!(f.events(), base, "nothing changed");
    f.write(&note(A, "NOTE/Note/a.note", &[("P1", Some(4), true)]), 10);
    f.notes.refresh();
    assert_eq!(f.events(), base + 1);
    // Opening it changes New notes.
    f.notes.seen(A).unwrap();
    assert_eq!(f.events(), base + 2);
    f.notes.seen(A).unwrap();
    assert_eq!(f.events(), base + 2);
    // A save without a change of what is listed: the time is listed, so it is a change.
    f.write(&note(A, "NOTE/Note/a.note", &[("P1", Some(4), true)]), 20);
    f.notes.refresh();
    assert_eq!(f.events(), base + 3);
    // A note without text changes nothing the windows show.
    f.write(&note(B, "NOTE/Note/b.note", &[("P1", None, false)]), 20);
    f.notes.refresh();
    assert_eq!(f.events(), base + 3);
    // Another data folder: the index starts over.
    let other = tempfile::tempdir().unwrap();
    *lock(&f.data_dir) = other.path().to_path_buf();
    f.notes.refresh();
    assert_eq!(f.events(), base + 4);
    assert!(f.notes.all().is_empty());
    // Back, then log out: the index starts over again each time.
    *lock(&f.data_dir) = f.data.path().to_path_buf();
    f.notes.refresh();
    assert_eq!(f.events(), base + 5);
    assert_eq!(f.all_keys(), [A]);
    *lock(&f.account) = None;
    f.notes.refresh();
    assert_eq!(f.events(), base + 6);
}

#[test]
fn an_account_that_is_no_plain_name_reads_nothing() {
    for bad in ["..", "../x", "a/b", ".hidden", ""] {
        let f = Fixture::new();
        f.write(&note(A, "NOTE/Note/a.note", &[("P1", Some(4), true)]), 10);
        *lock(&f.account) = Some(bad.to_owned());
        assert!(f.notes.all().is_empty(), "{bad}");
    }
}

#[test]
fn markers_belong_to_the_data_folder() {
    let f = Fixture::new();
    // A folder that is there but holds no store of the account sets no baseline.
    let bare = tempfile::tempdir().unwrap();
    let real = lock(&f.data_dir).clone();
    *lock(&f.data_dir) = bare.path().to_path_buf();
    assert!(f.new_keys().is_empty());
    assert!(!f.config.path().join("seen.json").exists());
    // Then the real store: its notes with text are the baseline, nothing is new.
    *lock(&f.data_dir) = real.clone();
    f.write(&note(A, "NOTE/Note/a.note", &[("P1", Some(4), true)]), 10);
    f.write(&note(B, "NOTE/Note/b.note", &[("P1", Some(5), true)]), 10);
    assert!(f.new_keys().is_empty());
    assert_eq!(f.marks(), json!({A: 4, B: 5}));
    // A new note, then a second store: it gets its own baseline, the first keeps its markers.
    f.write(&note(C, "NOTE/Note/c.note", &[("P1", Some(7), true)]), 20);
    assert_eq!(f.new_keys(), [C]);
    let second = tempfile::tempdir().unwrap();
    *lock(&f.data_dir) = second.path().to_path_buf();
    fs::create_dir_all(f.notes_dir()).unwrap();
    f.write(&note(B, "NOTE/Note/b.note", &[("P1", Some(9), true)]), 10);
    assert!(f.new_keys().is_empty());
    assert_eq!(f.marks(), json!({B: 9}));
    assert_eq!(marks_in(&f.seen_file(), &real), json!({A: 4, B: 5}));
    // Back: the old markers are intact, and C is still new.
    *lock(&f.data_dir) = real.clone();
    assert_eq!(f.new_keys(), [C]);
    assert_eq!(f.marks(), json!({A: 4, B: 5}));
    // At most three folders per account are kept: the one used longest ago goes.
    let (third, fourth) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    for dir in [third.path(), fourth.path()] {
        *lock(&f.data_dir) = dir.to_path_buf();
        fs::create_dir_all(f.notes_dir()).unwrap();
        f.notes.refresh();
    }
    let seen = f.seen_file();
    assert_eq!(seen[ACCOUNT].as_array().unwrap().len(), 3);
    assert_eq!(marks_in(&seen, second.path()), Value::Null);
    assert_eq!(marks_in(&seen, &real), json!({A: 4, B: 5}));
}

#[test]
fn a_note_that_comes_back_is_not_new_for_seen_text() {
    let f = after_baseline();
    f.write(&note(A, "NOTE/Note/a.note", &[("P1", Some(4), true)]), 10);
    f.write(&note(B, "NOTE/Note/b.note", &[("P1", Some(5), true)]), 10);
    f.notes.seen(A).unwrap();
    f.notes.seen(B).unwrap();
    // A goes to the bin, B leaves the scope.
    let mut deleted = note(A, "NOTE/Note/a.note", &[("P1", Some(4), true)]);
    deleted["deleted"] = json!(true);
    f.write(&deleted, 20);
    let mut unscoped = note(B, "NOTE/Note/b.note", &[("P1", Some(5), true)]);
    unscoped["unscoped"] = json!(true);
    f.write(&unscoped, 20);
    f.notes.refresh();
    // Both come back, read again with new seqs and the same text.
    f.write(&note(A, "NOTE/Note/a.note", &[("P1", Some(11), true)]), 30);
    f.write(&note(B, "NOTE/Note/b.note", &[("P1", Some(12), true)]), 30);
    assert!(f.new_keys().is_empty());
    assert_eq!(f.marks(), json!({A: 11, B: 12}));
    // New text after the return is new.
    f.write(
        &note(
            A,
            "NOTE/Note/a.note",
            &[("P1", Some(11), true), ("P2", Some(13), true)],
        ),
        40,
    );
    assert_eq!(f.new_keys(), [A]);
    // A note whose text was not seen before it went stays new when it comes back.
    f.write(&note(C, "NOTE/Note/c.note", &[("P1", Some(14), true)]), 50);
    let mut gone = note(C, "NOTE/Note/c.note", &[("P1", Some(14), true)]);
    gone["deleted"] = json!(true);
    f.write(&gone, 60);
    f.notes.refresh();
    f.write(&note(C, "NOTE/Note/c.note", &[("P1", Some(15), true)]), 70);
    assert_eq!(f.new_keys(), [C, A]);
}

/// Makes a file unreadable for this user; `None` when the tests run as root, which reads it
/// anyway.
fn unreadable(path: &Path) -> Option<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
    fs::read(path).is_err().then_some(())
}

fn readable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
}

#[test]
fn a_seen_file_that_cannot_be_read_is_never_overwritten() {
    let mine = json!({ACCOUNT: [{"dataDir": "/x", "seen": {"X": 3}}]});
    let f = Fixture::with_seen(Some(mine.clone()));
    let path = f.config.path().join("seen.json");
    if unreadable(&path).is_none() {
        return;
    }
    f.write(&note(A, "NOTE/Note/a.note", &[("P1", Some(4), true)]), 10);
    assert!(f.new_keys().is_empty());
    assert_eq!(f.notes.seen(A), Err(SEEN_NOT_READ.to_owned()));
    f.notes.refresh();
    readable(&path);
    assert_eq!(f.seen_file(), mine);
    assert!(!f.config.path().join("seen.json.tmp").exists());
    // Once it can be read, the markers load and the baseline is set.
    assert!(f.new_keys().is_empty());
    assert_eq!(f.marks(), json!({A: 4}));
    assert_eq!(marks_in(&f.seen_file(), Path::new("/x")), json!({"X": 3}));
}

#[test]
fn a_store_that_disappears_empties_the_index() {
    let f = after_baseline();
    f.write(&note(A, "NOTE/Note/a.note", &[("P1", Some(4), true)]), 10);
    f.notes.seen(A).unwrap();
    assert_eq!(f.all_keys(), [A]);
    let events = f.events();
    fs::remove_dir_all(f.notes_dir()).unwrap();
    assert!(f.notes.all().is_empty());
    assert!(f.notes.at_path("NOTE/Note/a.note").is_none());
    assert_eq!(f.events(), events + 1);
    // No pruning: the store comes back as it was.
    assert_eq!(f.marks(), json!({A: 4}));
    fs::create_dir_all(f.notes_dir()).unwrap();
    f.write(&note(A, "NOTE/Note/a.note", &[("P1", Some(4), true)]), 10);
    assert_eq!(f.all_keys(), [A]);
    assert!(f.new_keys().is_empty());
}

#[test]
fn a_file_that_cannot_be_read_for_a_moment_is_not_gone() {
    let f = after_baseline();
    f.write(&note(A, "NOTE/Note/a.note", &[("P1", Some(4), true)]), 10);
    f.notes.seen(A).unwrap();
    // The pipeline wrote it again, and it cannot be read for now.
    f.write(&note(A, "NOTE/Note/a.note", &[("P1", Some(4), true)]), 20);
    let path = f.file_of(A);
    if unreadable(&path).is_none() {
        return;
    }
    assert_eq!(f.all_keys(), [A]);
    assert_eq!(f.marks(), json!({A: 4}));
    // It is read again once it can be.
    readable(&path);
    assert_eq!(f.notes.all()[0].changed_ms, ms(20));
}

#[test]
fn notes_newer_than_counts_listed_notes_with_newer_text() {
    let f = Fixture::new();
    // Nothing yet: no count and no newest seq.
    assert_eq!(f.notes.notes_newer_than(0), (0, None));
    f.write(&note(A, "NOTE/Note/a.note", &[("P1", Some(4), true)]), 10);
    f.write(
        &note(
            B,
            "NOTE/Note/b.note",
            &[("P1", Some(5), true), ("P2", Some(9), true)],
        ),
        11,
    );
    // Not listed: no text yet, deleted, unscoped.
    f.write(&note(C, "NOTE/Note/c.note", &[("P1", Some(20), false)]), 12);
    let mut deleted = note("F5", "NOTE/Note/e.note", &[("P1", Some(21), true)]);
    deleted["deleted"] = json!(true);
    f.write(&deleted, 13);
    let mut unscoped = note("F6", "NOTE/Note/f.note", &[("P1", Some(22), true)]);
    unscoped["unscoped"] = json!(true);
    f.write(&unscoped, 14);
    // It scans first: the notes written above count without a refresh.
    assert_eq!(f.notes.notes_newer_than(0), (2, Some(9)));
    assert_eq!(f.notes.notes_newer_than(4), (1, Some(9)));
    assert_eq!(f.notes.notes_newer_than(9), (0, Some(9)));
    // The seen markers do not matter: an opened note still counts.
    f.notes.seen(B).unwrap();
    assert_eq!(f.notes.notes_newer_than(4), (1, Some(9)));
    // Without an account there are no notes.
    *lock(&f.account) = None;
    assert_eq!(f.notes.notes_newer_than(0), (0, None));
}
