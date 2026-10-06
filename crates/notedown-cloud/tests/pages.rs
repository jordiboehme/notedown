//! Page index: fingerprints, diffs and the page events of a commit.
//!
//! The local fixtures (bike, bike-digest) are private and gitignored; their tests skip with a
//! message when the files are missing. test1 and test2 are public.

use std::collections::BTreeMap;
use std::path::PathBuf;

use notedown_cloud::auth::Accounts;
use notedown_cloud::journal::{Event, EventBody, Journal, Origin, PartChange};
use notedown_cloud::pages::{
    self, PAGES_DIR, PageDiff, PageFingerprint, PageRecord, diff, fingerprint_note, note_key,
};
use notedown_cloud::store::{Store, WirePath};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn public(name: &str) -> Vec<u8> {
    std::fs::read(root().join(format!("tests/fixtures/notes/{name}.note"))).unwrap()
}

fn local(name: &str) -> Option<Vec<u8>> {
    let path = root().join(format!("tests/fixtures/local/{name}.note"));
    if !path.exists() {
        eprintln!("skipping: local fixture {} is missing", path.display());
        return None;
    }
    Some(std::fs::read(path).unwrap())
}

fn wp(s: &str) -> WirePath {
    WirePath::parse_named(s).unwrap()
}

struct Fixture {
    _dir: tempfile::TempDir,
    data: PathBuf,
    store: Store,
    journal: Journal,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().to_path_buf();
        Accounts::new(&data)
            .create("jordi", "jordi@example.org", "secret")
            .unwrap();
        let store = Store::open(&data, "jordi").unwrap();
        {
            let g = store.lock().unwrap();
            store.ensure_native_folders(&g).unwrap();
        }
        let journal = Journal::open(&data).unwrap();
        Fixture {
            _dir: dir,
            data,
            store,
            journal,
        }
    }

    fn stage(&self, bytes: &[u8]) -> PathBuf {
        let staging = self.store.account_dir().join("staging/u1");
        std::fs::create_dir_all(&staging).unwrap();
        let p = staging.join("f");
        std::fs::write(&p, bytes).unwrap();
        p
    }

    /// A put through the page indexer, as every writer does it. Returns the diff and the events
    /// the commit journaled.
    fn put_limit(&self, path: &str, bytes: &[u8], limit: u64) -> (Option<PageDiff>, Vec<Event>) {
        let before = self.journal.last_seq().unwrap();
        let g = self.store.lock_recovered(&self.journal).unwrap();
        let staged = self.stage(bytes);
        let mut c = self
            .store
            .plan_put(&g, &wp(path), &staged, Origin::Device, Some("SN1".into()))
            .unwrap();
        let diff = pages::plan_page_events_with_limit(&self.store, &g, &mut c, limit);
        let seqs = self.store.commit(&g, c, &self.journal).unwrap();
        let events: Vec<Event> = self
            .journal
            .read_from(before + 1)
            .unwrap()
            .map(|e| e.unwrap())
            .collect();
        assert_eq!(seqs.len(), events.len());
        (diff, events)
    }

    fn put(&self, path: &str, bytes: &[u8]) -> (Option<PageDiff>, Vec<Event>) {
        self.put_limit(path, bytes, pages::MAX_NOTE_BYTES)
    }

    fn record(&self, file_name: &str) -> Option<PageRecord> {
        let p = self
            .store
            .account_dir()
            .join(PAGES_DIR)
            .join(format!("{file_name}.json"));
        let bytes = std::fs::read(p).ok()?;
        Some(serde_json::from_slice(&bytes).unwrap())
    }
}

fn types(events: &[Event]) -> Vec<&'static str> {
    events.iter().map(|e| e.body.type_name()).collect()
}

fn pages_changed(e: &Event) -> Option<u64> {
    match &e.body {
        EventBody::FileCreated { pages_changed, .. }
        | EventBody::FileModified { pages_changed, .. } => *pages_changed,
        other => panic!("not a file event: {other:?}"),
    }
}

// ---------------------------------------------------------------- fingerprints

#[test]
fn test1_fingerprint_hashes_the_raw_blocks() {
    let bytes = public("test1");
    let fp = fingerprint_note(&bytes).unwrap();
    let nb = notedown_note::Notebook::from_bytes(
        bytes.clone(),
        notedown_note::VersionPolicy::AllowUnknown,
    )
    .unwrap();
    assert_eq!(fp.file_id, nb.file_id);
    assert_eq!(fp.pages.len(), nb.pages.len());
    let page = &nb.pages[0];
    let p = &fp.pages[0];
    assert_eq!(Some(p.page_id.clone()), page.id);
    assert_eq!(p.position, 0);
    // strokes: the first 16 hex digits of the SHA-256 of the TOTALPATH payload
    let totalpath = nb.block(page.totalpath.unwrap().address).unwrap();
    assert_eq!(p.strokes, pages::short_hash(totalpath));
    // layers: one hash per layer, by its page key
    let names: Vec<&str> = p.layers.keys().map(String::as_str).collect();
    let mut expect: Vec<&str> = page.layers.iter().map(|l| l.name.as_str()).collect();
    expect.sort();
    assert_eq!(names, expect);
    let main = page.layers.iter().find(|l| l.name == "MAINLAYER").unwrap();
    assert_eq!(
        p.layers["MAINLAYER"],
        pages::short_hash(nb.block(main.bitmap.unwrap().address).unwrap())
    );
    for h in [
        &p.strokes,
        &p.template,
        &p.textboxes,
        &p.recognition,
        &p.total,
    ] {
        assert_eq!(h.len(), 16);
        assert!(
            h.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
    }
    // deterministic
    assert_eq!(fingerprint_note(&bytes).unwrap(), fp);
}

#[test]
fn bike_strokes_hash_matches_the_published_sha256() {
    let Some(bytes) = local("bike") else {
        return;
    };
    let fp = fingerprint_note(&bytes).unwrap();
    assert_eq!(fp.pages.len(), 3);
    // docs/note-format.md 11.3: sha256 of the TOTALPATH blocks
    assert_eq!(fp.pages[0].strokes, "9ac691b048787dd6");
    assert_eq!(fp.pages[1].strokes, "00edacc7c17f0d0c");
    assert_eq!(fp.pages[2].strokes, "0b468183c96857f7");
    assert_eq!(
        fp.file_id.as_deref(),
        Some("F20240708174639550429bHksqaP2wKQ8")
    );
}

#[test]
fn the_total_leaves_out_page_id_and_position() {
    let a = PageFingerprint::new("P1", 0, "s", layers(&[("MAINLAYER", "m")]), "t", "x", "r");
    let b = PageFingerprint::new("P2", 7, "s", layers(&[("MAINLAYER", "m")]), "t", "x", "r");
    assert_eq!(a.total, b.total);
    let c = PageFingerprint::new("P1", 0, "s", layers(&[("MAINLAYER", "n")]), "t", "x", "r");
    assert_ne!(a.total, c.total);
    // the part hashes are not interchangeable
    let d = PageFingerprint::new("P1", 0, "t", layers(&[("MAINLAYER", "m")]), "s", "x", "r");
    assert_ne!(a.total, d.total);
}

#[test]
fn note_keys_are_safe_file_names() {
    let path = wp("NOTE/Note/a.note");
    assert_eq!(
        note_key(Some("F20240708174639550429bHksqaP2wKQ8"), &path),
        "F20240708174639550429bHksqaP2wKQ8"
    );
    let md5 = {
        use md5::{Digest, Md5};
        const_hex::encode(Md5::digest(b"NOTE/Note/a.note"))
    };
    let by_path = format!("path:{md5}");
    assert_eq!(note_key(None, &path), by_path);
    for bad in ["", "../x", "F1/../../x", "F 1", "x", "F", "F\u{e9}1"] {
        assert_eq!(note_key(Some(bad), &path), by_path, "{bad:?}");
    }
}

// ---------------------------------------------------------------- synthetic diffs

fn layers(items: &[(&str, &str)]) -> BTreeMap<String, String> {
    items
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// A page whose every part hash is derived from `content`.
fn page(id: &str, position: usize, content: &str) -> PageFingerprint {
    PageFingerprint::new(
        id,
        position,
        &format!("s-{content}"),
        layers(&[("MAINLAYER", &format!("m-{content}"))]),
        "t",
        "x",
        "r",
    )
}

fn pages_of(ids: &[(&str, &str)]) -> Vec<PageFingerprint> {
    ids.iter()
        .enumerate()
        .map(|(i, (id, c))| page(id, i, c))
        .collect()
}

fn event_summary(events: &[EventBody]) -> Vec<String> {
    events
        .iter()
        .map(|e| match e {
            EventBody::PageAdded {
                page_id, position, ..
            } => format!("added {page_id}@{position}"),
            EventBody::PageRemoved { page_id, .. } => format!("removed {page_id}"),
            EventBody::PageMoved {
                page_id, from, to, ..
            } => format!("moved {page_id} {from}->{to}"),
            EventBody::PageModified {
                page_id,
                position,
                parts,
                ..
            } => {
                let names: Vec<&str> = parts.iter().map(|p| p.name.as_str()).collect();
                format!("modified {page_id}@{position} {}", names.join(","))
            }
            other => format!("{other:?}"),
        })
        .collect()
}

#[test]
fn the_same_pages_give_no_change() {
    let a = pages_of(&[("A", "a"), ("B", "b")]);
    let d = diff(&a, &a);
    assert!(d.events(5).is_empty());
    assert_eq!(d.pages_changed(), Some(0));
}

#[test]
fn a_reorder_gives_moved_events() {
    let old = pages_of(&[("A", "a"), ("B", "b"), ("C", "c")]);
    let new = pages_of(&[("B", "b"), ("A", "a"), ("C", "c")]);
    let d = diff(&old, &new);
    assert_eq!(d.moved.len(), 2);
    assert!(d.added.is_empty() && d.removed.is_empty() && d.modified.is_empty());
    assert_eq!(
        event_summary(&d.events(5)),
        ["moved B 1->0", "moved A 0->1"]
    );
    assert_eq!(d.pages_changed(), Some(2));
    for e in d.events(5) {
        match e {
            EventBody::PageMoved { id, file_id, .. } => assert_eq!((id, file_id.as_str()), (5, "")),
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn an_insert_gives_added_and_moves_the_pages_after_it() {
    let old = pages_of(&[("A", "a"), ("B", "b")]);
    let new = pages_of(&[("A", "a"), ("X", "x"), ("B", "b")]);
    let d = diff(&old, &new);
    assert_eq!(event_summary(&d.events(1)), ["added X@1", "moved B 1->2"]);
}

#[test]
fn a_delete_gives_removed_and_moves_the_pages_after_it() {
    let old = pages_of(&[("A", "a"), ("B", "b"), ("C", "c")]);
    let new = pages_of(&[("A", "a"), ("C", "c")]);
    let d = diff(&old, &new);
    assert_eq!(d.removed.len(), 1);
    assert_eq!(event_summary(&d.events(1)), ["removed B", "moved C 2->1"]);
}

#[test]
fn a_changed_page_lists_its_parts_with_old_and_new_hashes() {
    let old = pages_of(&[("A", "a"), ("B", "b")]);
    let mut new = pages_of(&[("B", "b2"), ("A", "a")]);
    new[0] = PageFingerprint::new(
        "B",
        0,
        &new[0].strokes,
        new[0].layers.clone(),
        "t",
        "x2",
        "r",
    );
    let d = diff(&old, &new);
    assert_eq!(
        event_summary(&d.events(1)),
        [
            "moved B 1->0",
            "modified B@0 strokes,layers,textboxes",
            "moved A 0->1"
        ]
    );
    let parts = &d.modified[0].parts;
    assert_eq!(
        parts[0],
        PartChange {
            name: "strokes".into(),
            old: old[1].strokes.clone(),
            new: new[0].strokes.clone(),
        }
    );
    assert_eq!(parts[1].old, old[1].layers_hash());
    assert_eq!(parts[1].new, new[0].layers_hash());
    assert_eq!((parts[2].old.as_str(), parts[2].new.as_str()), ("x", "x2"));
}

#[test]
fn everything_new_or_gone() {
    let d = diff(&[], &pages_of(&[("A", "a"), ("B", "b")]));
    assert_eq!(event_summary(&d.events(1)), ["added A@0", "added B@1"]);
    let d = diff(&pages_of(&[("A", "a"), ("B", "b")]), &[]);
    assert_eq!(event_summary(&d.events(1)), ["removed A", "removed B"]);
}

// ---------------------------------------------------------------- commits

#[test]
fn a_new_note_journals_its_pages_right_after_the_file_event() {
    let f = Fixture::new();
    let bytes = public("test1");
    let fp = fingerprint_note(&bytes).unwrap();
    let (d, ev) = f.put("NOTE/Note/t1.note", &bytes);
    let d = d.unwrap();
    assert!(d.failed.is_none());
    let mut expect = vec!["file.created"];
    expect.extend(std::iter::repeat_n("page.added", fp.pages.len()));
    assert_eq!(types(&ev), expect);
    assert_eq!(pages_changed(&ev[0]), Some(fp.pages.len() as u64));
    // one commit: one opId, consecutive seqs, the same origin and device
    assert!(ev.iter().all(|e| e.op_id == ev[0].op_id
        && e.origin == Origin::Device
        && e.device.as_deref() == Some("SN1")));
    assert!(ev.windows(2).all(|w| w[1].seq == w[0].seq + 1));
    let id = f.store.id_of(&wp("NOTE/Note/t1.note")).unwrap().unwrap();
    let file_id = fp.file_id.clone().unwrap();
    match &ev[1].body {
        EventBody::PageAdded {
            id: eid,
            file_id: fid,
            page_id,
            position,
        } => {
            assert_eq!(
                (*eid, fid, page_id, *position),
                (id, &file_id, &fp.pages[0].page_id, 0)
            );
        }
        other => panic!("{other:?}"),
    }
    // the exact line of a page event
    let line = ev[1].to_line();
    let order = [
        "\"type\"",
        "\"id\"",
        "\"fileId\"",
        "\"pageId\"",
        "\"position\"",
        "\"opId\"",
    ];
    let at: Vec<usize> = order.iter().map(|k| line.find(k).unwrap()).collect();
    assert!(at.windows(2).all(|w| w[0] < w[1]), "{line}");
    assert!(line.contains("\"type\":\"page.added\""));
    // the page record
    let rec = f.record(&file_id).unwrap();
    assert_eq!(rec.file_id, file_id);
    assert_eq!(rec.path, "NOTE/Note/t1.note");
    assert_eq!(rec.pages, fp.pages);
    let text = std::fs::read_to_string(
        f.store
            .account_dir()
            .join(PAGES_DIR)
            .join(format!("{file_id}.json")),
    )
    .unwrap();
    assert!(text.starts_with("{\n  \"fileId\": "), "{text}");
    assert!(text.ends_with("}\n"));
    assert!(text.find("\"fileId\"") < text.find("\"path\""));
    assert!(text.find("\"path\"") < text.find("\"pages\""));
}

#[test]
fn the_same_note_twice_gives_no_page_events() {
    let f = Fixture::new();
    let bytes = public("test1");
    f.put("NOTE/Note/t1.note", &bytes);
    let (d, ev) = f.put("NOTE/Note/t1.note", &bytes);
    // an identical put is not a change at all: no file event, no page events
    assert!(d.is_none());
    assert!(ev.is_empty());
}

/// test1 with one byte flipped in a block the current state does not reach: the file changes,
/// its pages do not.
fn test1_with_a_stale_block_changed() -> Vec<u8> {
    let mut bytes = public("test1");
    let nb = notedown_note::Notebook::from_bytes(
        bytes.clone(),
        notedown_note::VersionPolicy::AllowUnknown,
    )
    .unwrap();
    let stale = nb
        .blocks
        .iter()
        .find(|b| !b.reachable && b.len > 0 && b.kind == notedown_note::BlockKind::Binary)
        .expect("test1 has a stale binary block");
    let at = stale.address as usize + 4;
    bytes[at] ^= 0xff;
    bytes
}

#[test]
fn a_changed_note_with_the_same_pages_gives_pages_changed_0() {
    let f = Fixture::new();
    f.put("NOTE/Note/t1.note", &public("test1"));
    let changed = test1_with_a_stale_block_changed();
    let (d, ev) = f.put("NOTE/Note/t1.note", &changed);
    assert_eq!(d.unwrap().pages_changed(), Some(0));
    assert_eq!(types(&ev), ["file.modified"]);
    assert_eq!(pages_changed(&ev[0]), Some(0));
    assert!(ev[0].to_line().contains("\"pagesChanged\":0"));
}

#[test]
fn bike_then_bike_digest_modifies_only_page_3() {
    let (Some(bike), Some(digest)) = (local("bike"), local("bike-digest")) else {
        return;
    };
    let f = Fixture::new();
    let (_, ev) = f.put("NOTE/Note/bike.note", &bike);
    assert_eq!(
        types(&ev),
        ["file.created", "page.added", "page.added", "page.added"]
    );
    let before = fingerprint_note(&bike).unwrap();
    let (d, ev) = f.put("NOTE/Note/bike.note", &digest);
    let d = d.unwrap();
    assert_eq!(types(&ev), ["file.modified", "page.modified"]);
    assert_eq!(pages_changed(&ev[0]), Some(1));
    let EventBody::PageModified {
        page_id,
        position,
        parts,
        file_id,
        ..
    } = &ev[1].body
    else {
        panic!("{:?}", ev[1]);
    };
    assert_eq!(*position, 2);
    assert_eq!(page_id, &before.pages[2].page_id);
    assert_eq!(file_id, "F20240708174639550429bHksqaP2wKQ8");
    let names: Vec<&str> = parts.iter().map(|p| p.name.as_str()).collect();
    // The device draws the text box into the MAINLAYER bitmap (docs/note-format.md 7.4,
    // 11.3a: a new bitmap block of 28802 bytes), so the raw layer bytes change as well.
    assert_eq!(names, ["strokes", "layers", "textboxes"]);
    assert_eq!(parts[0].old, "0b468183c96857f7");
    assert_eq!(parts[0].new, "d9dbdcb4ffc594f2");
    assert_eq!(d.modified.len(), 1);
    // the record now holds the new state
    let rec = f.record("F20240708174639550429bHksqaP2wKQ8").unwrap();
    assert_eq!(rec.pages, fingerprint_note(&digest).unwrap().pages);
}

#[test]
fn a_truncated_note_gives_index_failed_and_keeps_the_record() {
    let f = Fixture::new();
    let bytes = public("test2");
    f.put("NOTE/Note/t2.note", &bytes);
    let file_id = fingerprint_note(&bytes).unwrap().file_id.unwrap();
    let before = f.record(&file_id).unwrap();
    let truncated = &bytes[..bytes.len() / 2];
    let (d, ev) = f.put("NOTE/Note/t2.note", truncated);
    assert!(d.unwrap().failed.is_some());
    assert_eq!(types(&ev), ["file.modified", "page.index_failed"]);
    assert_eq!(pages_changed(&ev[0]), None);
    let id = f.store.id_of(&wp("NOTE/Note/t2.note")).unwrap().unwrap();
    match &ev[1].body {
        EventBody::PageIndexFailed {
            id: eid,
            path,
            reason,
        } => {
            assert_eq!((*eid, path.as_str()), (id, "NOTE/Note/t2.note"));
            assert!(!reason.is_empty());
        }
        other => panic!("{other:?}"),
    }
    // the data is in place; the record is not touched
    assert_eq!(
        std::fs::read(f.store.fs_path(&wp("NOTE/Note/t2.note"))).unwrap(),
        truncated
    );
    assert_eq!(f.record(&file_id).unwrap(), before);
    // the next good write diffs against the stored record: nothing changed
    let (_, ev) = f.put("NOTE/Note/t2.note", &bytes);
    assert_eq!(types(&ev), ["file.modified"]);
    assert_eq!(pages_changed(&ev[0]), Some(0));
}

#[test]
fn a_note_over_the_size_limit_gives_index_failed() {
    let f = Fixture::new();
    let bytes = public("test1");
    let (d, ev) = f.put_limit("NOTE/Note/big.note", &bytes, 1000);
    assert!(d.unwrap().failed.unwrap().contains("1000"));
    assert_eq!(types(&ev), ["file.created", "page.index_failed"]);
    assert!(!f.store.account_dir().join(PAGES_DIR).exists());
}

#[test]
fn other_files_get_no_page_events() {
    let f = Fixture::new();
    let (d, ev) = f.put("INBOX/t1.pdf", &public("test1"));
    assert!(d.is_none());
    assert_eq!(types(&ev), ["file.created"]);
    assert_eq!(pages_changed(&ev[0]), None);
}

#[test]
fn a_different_note_at_the_same_path_is_indexed_under_its_own_file_id() {
    let f = Fixture::new();
    f.put("NOTE/Note/n.note", &public("test1"));
    let t2 = public("test2");
    let fp = fingerprint_note(&t2).unwrap();
    let (_, ev) = f.put("NOTE/Note/n.note", &t2);
    let mut expect = vec!["file.modified"];
    expect.extend(std::iter::repeat_n("page.added", fp.pages.len()));
    assert_eq!(types(&ev), expect);
}

#[test]
fn snapshot_journals_the_pages_of_every_note() {
    let f = Fixture::new();
    let t1 = public("test1");
    let fp = fingerprint_note(&t1).unwrap();
    f.put("NOTE/Note/t1.note", &t1);
    f.put("NOTE/Note/bad.note", b"not a note");
    f.put("INBOX/a.pdf", b"pdf");
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let before = f.journal.last_seq().unwrap();
    f.store.snapshot(&g, &f.journal).unwrap();
    let ev: Vec<Event> = f
        .journal
        .read_from(before + 1)
        .unwrap()
        .map(|e| e.unwrap())
        .collect();
    assert!(
        ev.iter()
            .all(|e| e.origin == Origin::Cli && e.op_id == ev[0].op_id)
    );
    let t1_at = ev
        .iter()
        .position(|e| matches!(&e.body, EventBody::FileCreated { path, .. } if path == "NOTE/Note/t1.note"))
        .unwrap();
    assert_eq!(pages_changed(&ev[t1_at]), Some(fp.pages.len() as u64));
    let after: Vec<&str> = ev[t1_at + 1..=t1_at + fp.pages.len()]
        .iter()
        .map(|e| e.body.type_name())
        .collect();
    assert!(after.iter().all(|t| *t == "page.added"));
    let bad_at = ev
        .iter()
        .position(|e| matches!(&e.body, EventBody::FileCreated { path, .. } if path == "NOTE/Note/bad.note"))
        .unwrap();
    assert_eq!(pages_changed(&ev[bad_at]), None);
    assert_eq!(ev[bad_at + 1].body.type_name(), "page.index_failed");
    let pdf = ev
        .iter()
        .find(|e| matches!(&e.body, EventBody::FileCreated { path, .. } if path == "INBOX/a.pdf"))
        .unwrap();
    assert_eq!(pages_changed(pdf), None);
}

#[test]
fn a_crash_before_the_append_leaves_no_record_and_the_next_write_rediffs() {
    use notedown_cloud::store::CrashPoint;
    let f = Fixture::new();
    let bytes = public("test1");
    let file_id = fingerprint_note(&bytes).unwrap().file_id.unwrap();
    {
        let g = f.store.lock_recovered(&f.journal).unwrap();
        let staged = f.stage(&bytes);
        let mut c = f
            .store
            .plan_put(&g, &wp("NOTE/Note/t1.note"), &staged, Origin::Device, None)
            .unwrap();
        pages::plan_page_events(&f.store, &g, &mut c);
        f.store
            .commit_until(&g, c, &f.journal, Some(CrashPoint::BeforeAppend))
            .unwrap();
    }
    assert!(f.record(&file_id).is_none());
    // recovery journals the planned file and page events (from the intent)
    let store = Store::open(&f.data, "jordi").unwrap();
    let g = store.lock_recovered(&f.journal).unwrap();
    drop(g);
    let ev: Vec<Event> = f
        .journal
        .read_from(0)
        .unwrap()
        .map(|e| e.unwrap())
        .collect();
    assert_eq!(ev[0].body.type_name(), "file.created");
    assert!(ev[1..].iter().all(|e| e.body.type_name() == "page.added"));
    assert!(ev.iter().all(|e| e.origin == Origin::Repair));
    // the record is missing: harmless, the next write of the note diffs against nothing
    let (_, ev) = f.put("NOTE/Note/t1.note", &test1_with_a_stale_block_changed());
    assert_eq!(ev[0].body.type_name(), "file.modified");
    assert!(ev[1..].iter().all(|e| e.body.type_name() == "page.added"));
    assert!(f.record(&file_id).is_some());
}
