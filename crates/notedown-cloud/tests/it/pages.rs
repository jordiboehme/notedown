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
    let fp = fingerprint_note(bytes.clone()).unwrap();
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
    assert_eq!(fingerprint_note(bytes.clone()).unwrap(), fp);
}

#[test]
fn bike_strokes_hash_matches_the_published_sha256() {
    let Some(bytes) = local("bike") else {
        return;
    };
    let fp = fingerprint_note(bytes.clone()).unwrap();
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
fn a_swap_moves_one_of_the_two_pages() {
    let old = pages_of(&[("A", "a"), ("B", "b"), ("C", "c")]);
    let new = pages_of(&[("B", "b"), ("A", "a"), ("C", "c")]);
    let d = diff(&old, &new);
    assert_eq!(d.moved.len(), 1);
    assert!(d.added.is_empty() && d.removed.is_empty() && d.modified.is_empty());
    assert_eq!(event_summary(&d.events(5)), ["moved B 1->0"]);
    assert_eq!(d.pages_changed(), Some(1));
    for e in d.events(5) {
        match e {
            EventBody::PageMoved { id, file_id, .. } => assert_eq!((id, file_id.as_str()), (5, "")),
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn an_insert_moves_nothing() {
    let old = pages_of(&[("A", "a"), ("B", "b")]);
    let new = pages_of(&[("A", "a"), ("X", "x"), ("B", "b")]);
    assert_eq!(event_summary(&diff(&old, &new).events(1)), ["added X@1"]);
    // also at the front
    let new = pages_of(&[("X", "x"), ("A", "a"), ("B", "b")]);
    assert_eq!(event_summary(&diff(&old, &new).events(1)), ["added X@0"]);
}

#[test]
fn a_rotation_moves_one_page() {
    let old = pages_of(&[("A", "a"), ("B", "b"), ("C", "c"), ("D", "d")]);
    let new = pages_of(&[("B", "b"), ("C", "c"), ("D", "d"), ("A", "a")]);
    assert_eq!(event_summary(&diff(&old, &new).events(1)), ["moved A 0->3"]);
    let new = pages_of(&[("D", "d"), ("A", "a"), ("B", "b"), ("C", "c")]);
    assert_eq!(event_summary(&diff(&old, &new).events(1)), ["moved D 3->0"]);
}

#[test]
fn a_delete_moves_nothing() {
    let old = pages_of(&[("A", "a"), ("B", "b"), ("C", "c")]);
    let new = pages_of(&[("A", "a"), ("C", "c")]);
    let d = diff(&old, &new);
    assert_eq!(d.removed.len(), 1);
    assert_eq!(event_summary(&d.events(1)), ["removed B"]);
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
        ["moved B 1->0", "modified B@0 strokes,layers,textboxes"]
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
    let fp = fingerprint_note(bytes.clone()).unwrap();
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
    let before = fingerprint_note(bike.clone()).unwrap();
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
    assert_eq!(rec.pages, fingerprint_note(digest.clone()).unwrap().pages);
}

#[test]
fn a_truncated_note_gives_index_failed_and_keeps_the_record() {
    let f = Fixture::new();
    let bytes = public("test2");
    f.put("NOTE/Note/t2.note", &bytes);
    let file_id = fingerprint_note(bytes.clone()).unwrap().file_id.unwrap();
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
fn a_different_note_at_the_same_path_removes_the_old_pages() {
    let f = Fixture::new();
    let t1 = public("test1");
    let t2 = public("test2");
    let fp1 = fingerprint_note(t1.clone()).unwrap();
    let fp2 = fingerprint_note(t2.clone()).unwrap();
    let (id1, id2) = (fp1.file_id.clone().unwrap(), fp2.file_id.clone().unwrap());
    f.put("NOTE/Note/n.note", &t1);
    assert!(f.record(&id1).is_some());
    let (_, ev) = f.put("NOTE/Note/n.note", &t2);
    let mut expect = vec!["file.modified"];
    expect.extend(std::iter::repeat_n("page.removed", fp1.pages.len()));
    expect.extend(std::iter::repeat_n("page.added", fp2.pages.len()));
    assert_eq!(types(&ev), expect);
    assert_eq!(
        pages_changed(&ev[0]),
        Some((fp1.pages.len() + fp2.pages.len()) as u64)
    );
    match &ev[1].body {
        EventBody::PageRemoved {
            file_id, page_id, ..
        } => assert_eq!((file_id, page_id), (&id1, &fp1.pages[0].page_id)),
        other => panic!("{other:?}"),
    }
    match ev.last().map(|e| &e.body) {
        Some(EventBody::PageAdded { file_id, .. }) => assert_eq!(file_id, &id2),
        other => panic!("{other:?}"),
    }
    // the old record is dropped, the new one is in place
    assert!(f.record(&id1).is_none());
    assert_eq!(f.record(&id2).unwrap().path, "NOTE/Note/n.note");
}

#[test]
fn snapshot_journals_the_pages_of_every_note() {
    let f = Fixture::new();
    let t1 = public("test1");
    let fp = fingerprint_note(t1.clone()).unwrap();
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
            .all(|e| e.origin == Origin::Snapshot && e.op_id == ev[0].op_id)
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
    let file_id = fingerprint_note(bytes.clone()).unwrap().file_id.unwrap();
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
    // the record is missing: harmless, the next write diffs against the file on disk
    let (_, ev) = f.put("NOTE/Note/t1.note", &test1_with_a_stale_block_changed());
    assert_eq!(types(&ev), ["file.modified"]);
    assert_eq!(pages_changed(&ev[0]), Some(0));
    assert!(f.record(&file_id).is_some());
}

// ---------------------------------------------------------------- fix round 1

const FID: &str = "F20261006120000000000abcdefABCDEF";

fn block(data: &mut Vec<u8>, payload: &[u8]) -> u32 {
    let at = data.len() as u32;
    data.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    data.extend_from_slice(payload);
    at
}

/// A minimal note (docs/note-format.md 1 to 4): a header with `FILE_ID` (or none) and one page
/// block per `(PAGEID, PAGESTYLE)`. The style is the part a test changes (`template`).
fn synthetic(file_id: Option<&str>, pages: &[(Option<&str>, &str)]) -> Vec<u8> {
    let mut d = b"noteSN_FILE_VER_20230015".to_vec();
    let header = match file_id {
        Some(id) => format!("<FILE_ID:{id}><APPLY_EQUIPMENT:N6>"),
        None => "<APPLY_EQUIPMENT:N6>".to_string(),
    };
    let h = block(&mut d, header.as_bytes());
    let mut footer = format!("<FILE_FEATURE:{h}>");
    for (n, (id, style)) in pages.iter().enumerate() {
        let meta = match id {
            Some(id) => format!("<PAGEID:{id}><PAGESTYLE:{style}>"),
            None => format!("<PAGESTYLE:{style}>"),
        };
        let at = block(&mut d, meta.as_bytes());
        footer.push_str(&format!("<PAGE{}:{at}>", n + 1));
    }
    let f = block(&mut d, footer.as_bytes());
    d.extend_from_slice(b"tail");
    d.extend_from_slice(&f.to_le_bytes());
    d
}

/// Three pages A, B, C with the given styles.
fn abc(file_id: Option<&str>, styles: [&str; 3]) -> Vec<u8> {
    synthetic(
        file_id,
        &[
            (Some("PA"), styles[0]),
            (Some("PB"), styles[1]),
            (Some("PC"), styles[2]),
        ],
    )
}

fn md5_hex(bytes: &[u8]) -> String {
    use md5::{Digest, Md5};
    const_hex::encode(Md5::digest(bytes))
}

fn path_key_file(path: &str) -> String {
    format!("path-{}", md5_hex(path.as_bytes()))
}

impl Fixture {
    /// A put without the indexer: the record is not touched.
    fn put_raw(&self, path: &str, bytes: &[u8]) {
        let g = self.store.lock_recovered(&self.journal).unwrap();
        let staged = self.stage(bytes);
        let c = self
            .store
            .plan_put(&g, &wp(path), &staged, Origin::Cli, None)
            .unwrap();
        self.store.commit(&g, c, &self.journal).unwrap();
    }

    fn since(&self, before: u64) -> Vec<Event> {
        self.journal
            .read_from(before + 1)
            .unwrap()
            .map(|e| e.unwrap())
            .collect()
    }

    fn copy(&self, from: &str, to: &str) -> (Option<PageDiff>, Vec<Event>) {
        let before = self.journal.last_seq().unwrap();
        let g = self.store.lock_recovered(&self.journal).unwrap();
        let mut c = self
            .store
            .plan_copy(&g, &wp(from), &wp(to), Origin::Device, None)
            .unwrap();
        let d = pages::plan_page_events(&self.store, &g, &mut c);
        self.store.commit(&g, c, &self.journal).unwrap();
        (d, self.since(before))
    }

    fn mv(&self, from: &str, to: &str) -> Vec<Event> {
        let before = self.journal.last_seq().unwrap();
        let g = self.store.lock_recovered(&self.journal).unwrap();
        let mut c = self
            .store
            .plan_move(&g, &wp(from), &wp(to), Origin::Device, None)
            .unwrap();
        pages::plan_page_move(&self.store, &g, &mut c);
        assert!(pages::plan_page_events(&self.store, &g, &mut c).is_none());
        self.store.commit(&g, c, &self.journal).unwrap();
        self.since(before)
    }

    fn mkdir(&self, path: &str) {
        let g = self.store.lock_recovered(&self.journal).unwrap();
        let c = self
            .store
            .plan_create_folder(&g, &wp(path), Origin::Cli, None)
            .unwrap();
        self.store.commit(&g, c, &self.journal).unwrap();
    }

    fn record_path(&self, file_name: &str) -> PathBuf {
        self.store
            .account_dir()
            .join(PAGES_DIR)
            .join(format!("{file_name}.json"))
    }
}

fn summary(events: &[Event]) -> Vec<String> {
    let bodies: Vec<EventBody> = events.iter().map(|e| e.body.clone()).collect();
    event_summary(&bodies)
}

#[test]
fn snapshot_writes_the_records_so_the_next_edit_is_exact() {
    let f = Fixture::new();
    f.put_raw("NOTE/Note/n.note", &abc(Some(FID), ["s", "s", "s"]));
    assert!(f.record(FID).is_none());
    {
        let g = f.store.lock_recovered(&f.journal).unwrap();
        f.store.snapshot(&g, &f.journal).unwrap();
    }
    let rec = f.record(FID).unwrap();
    assert_eq!(rec.path, "NOTE/Note/n.note");
    let v1 = abc(Some(FID), ["s", "s", "s"]);
    assert_eq!(
        (rec.size, rec.md5.as_str()),
        (v1.len() as u64, md5_hex(&v1).as_str())
    );
    let (_, ev) = f.put("NOTE/Note/n.note", &abc(Some(FID), ["s", "t", "s"]));
    assert_eq!(types(&ev), ["file.modified", "page.modified"]);
    assert_eq!(summary(&ev[1..]), ["modified PB@1 template"]);
}

#[test]
fn a_stale_record_gives_only_the_real_change() {
    let f = Fixture::new();
    f.put("NOTE/Note/n.note", &abc(Some(FID), ["s", "s", "s"]));
    // a write the indexer did not see (a crash, an older server): the record is stale
    f.put_raw("NOTE/Note/n.note", &abc(Some(FID), ["t", "s", "s"]));
    let (_, ev) = f.put("NOTE/Note/n.note", &abc(Some(FID), ["t", "s", "u"]));
    assert_eq!(summary(&ev[1..]), ["modified PC@2 template"]);
    assert_eq!(pages_changed(&ev[0]), Some(1));
    // healed
    let v3 = abc(Some(FID), ["t", "s", "u"]);
    assert_eq!(f.record(FID).unwrap().md5, md5_hex(&v3));
}

#[test]
fn a_note_placed_before_the_indexer_ran_gives_only_the_real_change() {
    let f = Fixture::new();
    f.put_raw("NOTE/Note/n.note", &abc(Some(FID), ["s", "s", "s"]));
    let (_, ev) = f.put("NOTE/Note/n.note", &abc(Some(FID), ["s", "s", "t"]));
    assert_eq!(summary(&ev[1..]), ["modified PC@2 template"]);
}

#[test]
fn a_corrupt_record_counts_as_missing() {
    let f = Fixture::new();
    f.put("NOTE/Note/n.note", &abc(Some(FID), ["s", "s", "s"]));
    std::fs::write(f.record_path(FID), b"{not json").unwrap();
    // a warning on stderr; the baseline comes from the file on disk
    let (_, ev) = f.put("NOTE/Note/n.note", &abc(Some(FID), ["t", "s", "s"]));
    assert_eq!(summary(&ev[1..]), ["modified PA@0 template"]);
    assert_eq!(f.record(FID).unwrap().pages.len(), 3);
}

#[test]
fn a_copy_is_keyed_by_its_path() {
    let f = Fixture::new();
    f.put("NOTE/Note/a.note", &abc(Some(FID), ["s", "s", "s"]));
    let (d, ev) = f.copy("NOTE/Note/a.note", "NOTE/Note/b.note");
    let key = format!("path:{}", md5_hex(b"NOTE/Note/b.note"));
    assert_eq!(d.unwrap().file_id, key);
    assert_eq!(
        types(&ev),
        ["file.copied", "page.added", "page.added", "page.added"]
    );
    assert!(
        ev[1..]
            .iter()
            .all(|e| matches!(&e.body, EventBody::PageAdded { file_id, .. } if *file_id == key))
    );
    let copy = f.record(&path_key_file("NOTE/Note/b.note")).unwrap();
    assert_eq!(copy.file_id, key);
    assert_eq!(copy.note_id.as_deref(), Some(FID));
    assert_eq!(copy.path, "NOTE/Note/b.note");
    assert_eq!(f.record(FID).unwrap().path, "NOTE/Note/a.note");

    // an edit of the copy diffs against the copy, the original's record stays
    let (_, ev) = f.put("NOTE/Note/b.note", &abc(Some(FID), ["s", "t", "s"]));
    assert_eq!(summary(&ev[1..]), ["modified PB@1 template"]);
    assert!(matches!(&ev[1].body, EventBody::PageModified { file_id, .. } if *file_id == key));
    assert_eq!(f.record(FID).unwrap().path, "NOTE/Note/a.note");
    // and an edit of the original against the original
    let (_, ev) = f.put("NOTE/Note/a.note", &abc(Some(FID), ["s", "s", "u"]));
    assert_eq!(summary(&ev[1..]), ["modified PC@2 template"]);
    assert!(matches!(&ev[1].body, EventBody::PageModified { file_id, .. } if file_id == FID));
}

#[test]
fn a_move_carries_the_record_along() {
    let f = Fixture::new();
    f.put("NOTE/Note/a.note", &abc(Some(FID), ["s", "s", "s"]));
    let ev = f.mv("NOTE/Note/a.note", "NOTE/Note/c.note");
    assert_eq!(types(&ev), ["file.moved"]);
    assert_eq!(f.record(FID).unwrap().path, "NOTE/Note/c.note");

    // a folder move takes its notes along; a record keyed by path is re-keyed
    f.mkdir("NOTE/Note/D");
    f.put("NOTE/Note/D/x.note", &abc(None, ["s", "s", "s"]));
    assert!(f.record(&path_key_file("NOTE/Note/D/x.note")).is_some());
    f.mv("NOTE/Note/D", "NOTE/Note/E");
    assert!(f.record(&path_key_file("NOTE/Note/D/x.note")).is_none());
    let rec = f.record(&path_key_file("NOTE/Note/E/x.note")).unwrap();
    assert_eq!(rec.path, "NOTE/Note/E/x.note");
    assert_eq!(
        rec.file_id,
        format!("path:{}", md5_hex(b"NOTE/Note/E/x.note"))
    );
    let (_, ev) = f.put("NOTE/Note/E/x.note", &abc(None, ["s", "t", "s"]));
    assert_eq!(summary(&ev[1..]), ["modified PB@1 template"]);
}

#[test]
fn a_note_without_a_valid_file_id_is_keyed_by_its_path() {
    let f = Fixture::new();
    for (path, id) in [
        ("NOTE/Note/none.note", None),
        ("NOTE/Note/bad.note", Some("G123")),
    ] {
        let (_, ev) = f.put(path, &abc(id, ["s", "s", "s"]));
        let key = format!("path:{}", md5_hex(path.as_bytes()));
        assert_eq!(types(&ev)[1..], ["page.added"; 3]);
        assert!(
            ev[1..].iter().all(
                |e| matches!(&e.body, EventBody::PageAdded { file_id, .. } if *file_id == key)
            )
        );
        let rec = f.record(&path_key_file(path)).unwrap();
        assert_eq!((rec.file_id, rec.note_id), (key, None));
        let (_, ev) = f.put(path, &abc(id, ["s", "s", "t"]));
        assert_eq!(summary(&ev[1..]), ["modified PC@2 template"]);
    }
}

#[test]
fn duplicate_and_missing_page_ids_get_stable_ids() {
    let bytes = synthetic(
        Some(FID),
        &[
            (None, "s"),
            (Some("P1"), "s"),
            (Some("P1"), "s"),
            (None, "s"),
        ],
    );
    let fp = fingerprint_note(bytes.clone()).unwrap();
    let ids: Vec<&str> = fp.pages.iter().map(|p| p.page_id.as_str()).collect();
    assert_eq!(ids, ["position:0", "P1", "P1#2", "position:3"]);
    let f = Fixture::new();
    f.put("NOTE/Note/n.note", &bytes);
    let (_, ev) = f.put(
        "NOTE/Note/n.note",
        &synthetic(
            Some(FID),
            &[
                (None, "s"),
                (Some("P1"), "s"),
                (Some("P1"), "t"),
                (None, "s"),
            ],
        ),
    );
    assert_eq!(summary(&ev[1..]), ["modified P1#2@2 template"]);
}

#[test]
fn index_note_compares_with_the_file_there_and_writes_nothing() {
    let f = Fixture::new();
    let v1 = abc(Some(FID), ["s", "s", "s"]);
    f.put("NOTE/Note/n.note", &v1);
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let v2 = abc(Some(FID), ["s", "s", "t"]);
    let d = pages::index_note(&f.store, &g, &wp("NOTE/Note/n.note"), v2.clone());
    assert_eq!(d.file_id, FID);
    assert_eq!(d.modified.len(), 1);
    assert_eq!(d.pages_changed(), Some(1));
    let rec = d.record.unwrap();
    assert_eq!(rec.md5, md5_hex(&v2));
    // nothing written
    assert_eq!(f.record(FID).unwrap().md5, md5_hex(&v1));
    // a new path: every page is added
    let d = pages::index_note(&f.store, &g, &wp("NOTE/Note/m.note"), v2);
    assert_eq!(d.added.len(), 3);
    assert!(d.removed.is_empty());
    // not a note
    let d = pages::index_note(&f.store, &g, &wp("NOTE/Note/m.note"), b"x".to_vec());
    assert!(d.failed.is_some());
}

// ---------------------------------------------------------------- fix round 2

/// Every file below the data folder outside `pages/`, with its content.
fn files_outside_pages(f: &Fixture) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &std::path::Path, pages: &std::path::Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p == pages {
                continue;
            }
            if p.is_dir() {
                walk(&p, pages, out);
            } else {
                out.insert(p.clone(), std::fs::read(&p).unwrap_or_default());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(&f.data, &f.store.account_dir().join(PAGES_DIR), &mut out);
    out
}

/// Replace the record of `FID` with one whose `fileId` or `noteId` is a path.
fn poison_record(f: &Fixture, path: &str, field: &str, value: &str) {
    let p = f.record_path(FID);
    let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
    v[field] = serde_json::Value::String(value.into());
    v["path"] = serde_json::Value::String(path.into());
    std::fs::write(&p, serde_json::to_vec(&v).unwrap()).unwrap();
}

#[test]
fn record_content_never_names_a_file() {
    for (field, value) in [
        ("fileId", "../../evil"),
        ("noteId", "../x"),
        ("fileId", "path:../../x"),
    ] {
        let f = Fixture::new();
        // sentinels where a path built from the record would land
        let accounts = f.store.account_dir().parent().unwrap().to_path_buf();
        std::fs::write(accounts.join("evil.json"), b"keep").unwrap();
        std::fs::write(f.store.account_dir().join("x.json"), b"keep").unwrap();

        f.put("NOTE/Note/a.note", &abc(Some(FID), ["s", "s", "s"]));
        poison_record(&f, "NOTE/Note/a.note", field, value);
        let mut outside = files_outside_pages(&f);

        // the poisoned record counts as missing: the move carries nothing
        f.mv("NOTE/Note/a.note", "NOTE/Note/c.note");
        // a put heals from the file on disk: only the real change
        let (_, ev) = f.put("NOTE/Note/c.note", &abc(Some(FID), ["s", "t", "s"]));
        assert_eq!(summary(&ev[1..]), ["modified PB@1 template"], "{field}");
        poison_record(&f, "NOTE/Note/c.note", field, value);
        // a replace by another note
        let (_, ev) = f.put(
            "NOTE/Note/c.note",
            &abc(Some("F20261006120000000000zzzzzzZZZZZZ"), ["s", "s", "s"]),
        );
        assert_eq!(types(&ev)[1..4], ["page.removed"; 3], "{field}");

        // nothing outside pages/ changed except the moved and written note itself
        let after = files_outside_pages(&f);
        assert_eq!(after.get(&accounts.join("evil.json")).unwrap(), b"keep");
        assert_eq!(
            after.get(&f.store.account_dir().join("x.json")).unwrap(),
            b"keep"
        );
        let moved = f.store.fs_path(&wp("NOTE/Note/a.note"));
        let target = f.store.fs_path(&wp("NOTE/Note/c.note"));
        outside.remove(&moved);
        let new_paths: Vec<&PathBuf> = after
            .keys()
            .filter(|p| !outside.contains_key(*p) && **p != target)
            .filter(|p| !p.starts_with(f.data.join("journal")))
            .filter(|p| {
                let n = p.file_name().unwrap().to_string_lossy();
                !(n.starts_with("ids") || n == "hashes.json")
            })
            .collect();
        assert!(new_paths.is_empty(), "{field}: {new_paths:?}");
    }
}

#[test]
fn a_side_write_outside_pages_is_refused() {
    use notedown_cloud::store::SideWrite;
    let f = Fixture::new();
    let acct = f.store.account_dir().to_path_buf();
    std::fs::write(acct.join("keep.txt"), b"keep").unwrap();
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let staged = f.stage(b"pdf");
    let mut c = f
        .store
        .plan_put(&g, &wp("INBOX/a.pdf"), &staged, Origin::Cli, None)
        .unwrap();
    let bad = [
        acct.join("pages/../../evil.json"),
        acct.join("pages/../keep.txt"),
        acct.join("keep.txt"),
        acct.join("pages/sub/x.json"),
        acct.join("pages"),
        acct.join("../jordi/pages/x.json"),
    ];
    for p in &bad {
        c.side_writes.push(SideWrite {
            path: p.clone(),
            bytes: Some(b"bad".to_vec()),
        });
        c.side_writes.push(SideWrite {
            path: p.clone(),
            bytes: None,
        });
    }
    c.side_writes.push(SideWrite {
        path: acct.join("pages/ok.json"),
        bytes: Some(b"ok".to_vec()),
    });
    // a refused side write never fails the commit
    f.store.commit(&g, c, &f.journal).unwrap();
    assert_eq!(std::fs::read(acct.join("keep.txt")).unwrap(), b"keep");
    assert!(!acct.parent().unwrap().join("evil.json").exists());
    assert!(!acct.join("pages/sub").exists());
    assert_eq!(std::fs::read(acct.join("pages/ok.json")).unwrap(), b"ok");
    assert_eq!(
        std::fs::read(f.store.fs_path(&wp("INBOX/a.pdf"))).unwrap(),
        b"pdf"
    );
}

#[test]
fn keys_are_valid_file_ids_or_path_keys() {
    assert!(pages::valid_key(FID));
    assert!(pages::valid_key(&format!("path:{}", md5_hex(b"x"))));
    for bad in [
        "",
        "../x",
        "path:../../x",
        "path:ABCDEF0123456789ABCDEF0123456789",
        "path:0123",
        "G1",
        "F",
    ] {
        assert!(!pages::valid_key(bad), "{bad:?}");
    }
}
