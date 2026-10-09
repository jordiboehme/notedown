use super::*;
use crate::model::{Block, LineChange, PageOcrState, PageState};
use crate::page::StrokeEntry;

const KEY: &str = "F20240424175151947577oMie9HNQYnTi";
const OTHER: &str = "F20240424175151947577oMie9HNQYnTj";

fn block(id: &str, text: &str) -> Block {
    Block {
        id: id.into(),
        label: "text".into(),
        bbox: [0, 0, 10, 10],
        text: text.into(),
        replaces: Vec::new(),
    }
}

fn page_line(note: &str, page: &str, change: LineChange, blocks: Vec<Block>) -> LogLine {
    let mut line = LogLine::new(note, change);
    line.page_id = Some(page.into());
    line.page_hash = Some(format!("hash-{}", blocks.len()));
    line.blocks = blocks;
    line
}

fn seqs(store: &TextStore, from: u64, up_to: Option<u64>) -> Vec<u64> {
    let mut out = Vec::new();
    store
        .read_log(from, up_to, &mut |l| {
            out.push(l.seq);
            Ok(())
        })
        .unwrap();
    out
}

fn appended(store: &TextStore, w: &WriterLock, n: usize) {
    let mut lines: Vec<LogLine> = (0..n)
        .map(|i| page_line(KEY, &format!("P{i}"), LineChange::New, vec![]))
        .collect();
    store.append(w, &mut lines).unwrap();
}

#[test]
fn seq_grows_and_survives_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    {
        let w = WriterLock::take(tmp.path()).unwrap();
        let store = TextStore::open(tmp.path(), "jordi").unwrap();
        assert!(store.dir().is_dir());
        assert_eq!(store.last_seq().unwrap(), 0);
        let mut lines = vec![
            page_line(KEY, "P1", LineChange::New, vec![block("b1", "one")]),
            page_line(KEY, "P2", LineChange::New, vec![block("b2", "two")]),
        ];
        store.append(&w, &mut lines).unwrap();
        assert_eq!(lines[0].seq, 1);
        assert_eq!(lines[1].seq, 2);
        assert!(lines[0].ts > 0, "the append stamps the time");
    }
    let w = WriterLock::take(tmp.path()).unwrap();
    let store = TextStore::open(tmp.path(), "jordi").unwrap();
    assert_eq!(store.last_seq().unwrap(), 2);
    let mut lines = vec![page_line(KEY, "P1", LineChange::Edited, vec![])];
    store.append(&w, &mut lines).unwrap();
    assert_eq!(lines[0].seq, 3);
    assert_eq!(seqs(&store, 0, None), vec![1, 2, 3]);
    let mut texts = Vec::new();
    store
        .read_log(0, Some(1), &mut |l| {
            texts.push(l.blocks[0].text.clone());
            Ok(())
        })
        .unwrap();
    assert_eq!(texts, vec!["one".to_string()]);
}

#[test]
fn a_torn_last_line_is_cut_at_start() {
    let tmp = tempfile::tempdir().unwrap();
    let w = WriterLock::take(tmp.path()).unwrap();
    let store = TextStore::open(tmp.path(), "jordi").unwrap();
    appended(&store, &w, 2);
    let log = store.dir().join("log.jsonl");
    let whole = std::fs::read(&log).unwrap();
    let mut torn = whole.clone();
    torn.extend_from_slice(br#"{"seq":3,"ts":1,"note":"F20"#);
    std::fs::write(&log, &torn).unwrap();

    // Readers stop at the last complete line.
    assert_eq!(seqs(&store, 0, None), vec![1, 2]);
    assert_eq!(store.last_seq().unwrap(), 2);

    store.repair(&w).unwrap();
    assert_eq!(std::fs::read(&log).unwrap(), whole);
    appended(&store, &w, 1);
    assert_eq!(seqs(&store, 0, None), vec![1, 2, 3]);
}

#[test]
fn an_append_after_a_torn_line_cuts_it_first() {
    let tmp = tempfile::tempdir().unwrap();
    let w = WriterLock::take(tmp.path()).unwrap();
    let store = TextStore::open(tmp.path(), "jordi").unwrap();
    appended(&store, &w, 1);
    let log = store.dir().join("log.jsonl");
    let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
    std::io::Write::write_all(&mut f, b"{\"seq\":2").unwrap();
    appended(&store, &w, 1);
    assert_eq!(seqs(&store, 0, None), vec![1, 2]);
    let text = std::fs::read_to_string(&log).unwrap();
    assert_eq!(text.lines().count(), 2, "{text}");
}

fn first_state() -> NoteState {
    NoteState {
        key: KEY.into(),
        path: "NOTE/Note/Plan.note".into(),
        title: "Plan".into(),
        size: 10,
        md5: "m".into(),
        pages: vec![PageState {
            page_id: "P1".into(),
            position: 0,
            page_hash: "h1".into(),
            strokes_hash: "s1".into(),
            layers_hash: "l1".into(),
            seq: Some(1),
            state: PageOcrState::Done,
            stroke_list: vec![StrokeEntry {
                hash: "abc".into(),
                bbox: [1, 2, 3, 4],
            }],
            blocks: vec![block("b1", "hello")],
        }],
        next_block: 2,
        ..NoteState::default()
    }
}

#[test]
fn a_crash_between_log_and_state_is_reconciled() {
    let tmp = tempfile::tempdir().unwrap();
    let other = NoteState {
        key: OTHER.into(),
        path: "NOTE/Note/Other.note".into(),
        title: "Other".into(),
        ..NoteState::default()
    };
    {
        let w = WriterLock::take(tmp.path()).unwrap();
        let store = TextStore::open(tmp.path(), "jordi").unwrap();
        let mut first = vec![page_line(
            KEY,
            "P1",
            LineChange::New,
            vec![block("b1", "hello")],
        )];
        store.append(&w, &mut first).unwrap();
        store.save_note(&w, &first_state()).unwrap();
        store.save_note(&w, &other).unwrap();

        // The second version: logged, then the process dies before the state is saved.
        let mut b4 = block("b4", "hello world");
        b4.replaces = vec!["b1".into()];
        let mut second = vec![page_line(KEY, "P1", LineChange::Edited, vec![b4])];
        second[0].page_hash = Some("h2".into());
        let mut moved = LogLine::new(KEY, LineChange::NoteMoved);
        moved.path = Some("NOTE/Note/Work/Plan2.note".into());
        moved.old_path = Some("NOTE/Note/Plan.note".into());
        second.push(moved);
        store.append(&w, &mut second).unwrap();
    }
    let w = WriterLock::take(tmp.path()).unwrap();
    let store = TextStore::open(tmp.path(), "jordi").unwrap();
    let saved = NoteState {
        log_seq: 1,
        ..first_state()
    };
    assert_eq!(store.note(KEY).unwrap().unwrap(), saved);
    store.repair(&w).unwrap();

    let state = store.note(KEY).unwrap().unwrap();
    let page = &state.pages[0];
    assert_eq!(page.seq, Some(2));
    assert_eq!(page.blocks[0].text, "hello world");
    assert_eq!(page.page_hash, "h2");
    assert_eq!(page.state, PageOcrState::Done);
    // What the log does not hold stays, so the worker checks the page again.
    assert_eq!(page.strokes_hash, "s1");
    assert_eq!(page.stroke_list, first_state().pages[0].stroke_list);
    assert_eq!(state.next_block, 5, "b4 is taken");
    assert_eq!(state.successors.get("b1"), Some(&vec!["b4".to_string()]));
    assert_eq!(state.path, "NOTE/Note/Work/Plan2.note");
    assert_eq!(state.title, "Plan2");
    assert_eq!(
        state.log_seq, 3,
        "the repaired state has seen the whole log"
    );
    assert_eq!(
        store.note(OTHER).unwrap().unwrap(),
        NoteState {
            log_seq: 1,
            ..other
        }
    );

    // A second repair changes nothing.
    store.repair(&w).unwrap();
    assert_eq!(store.note(KEY).unwrap().unwrap(), state);
}

#[test]
fn repair_applies_page_deletes_and_moves_and_never_reads_unscoped_as_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let w = WriterLock::take(tmp.path()).unwrap();
    let store = TextStore::open(tmp.path(), "jordi").unwrap();
    let mut state = first_state();
    state.pages.push(PageState {
        page_id: "P2".into(),
        position: 1,
        seq: Some(1),
        ..PageState::default()
    });
    store
        .append(
            &w,
            &mut [page_line(
                KEY,
                "P1",
                LineChange::New,
                vec![block("b1", "hello")],
            )],
        )
        .unwrap();
    store.save_note(&w, &state).unwrap();
    let mut gone = page_line(KEY, "P2", LineChange::Deleted, vec![]);
    gone.page_hash = None;
    let mut moved = page_line(KEY, "P1", LineChange::Moved, vec![]);
    moved.page_hash = None;
    moved.position = Some(3);
    let unscoped = LogLine::new(KEY, LineChange::NoteUnscoped);
    store.append(&w, &mut [gone, moved, unscoped]).unwrap();
    store.repair(&w).unwrap();
    let state = store.note(KEY).unwrap().unwrap();
    assert_eq!(state.pages.len(), 1);
    assert_eq!(state.pages[0].position, 3);
    assert!(!state.deleted);

    store
        .append(&w, &mut [LogLine::new(KEY, LineChange::NoteDeleted)])
        .unwrap();
    store.repair(&w).unwrap();
    assert!(store.note(KEY).unwrap().unwrap().deleted);
}

#[test]
fn a_second_writer_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let w = WriterLock::take(tmp.path()).unwrap();
    let err = WriterLock::take(tmp.path()).expect_err("a second writer");
    assert!(matches!(err, crate::Error::WriterBusy), "{err:?}");
    assert_eq!(
        err.to_string(),
        "another notedown pipeline runs for this data folder"
    );
    assert!(tmp.path().join("pipeline/.writer.lock").is_file());
    let store = TextStore::open(tmp.path(), "jordi").unwrap();
    appended(&store, &w, 2);
    drop(w);

    let w = WriterLock::take(tmp.path()).unwrap();
    appended(&store, &w, 2);
    assert_eq!(seqs(&store, 0, None), vec![1, 2, 3, 4]);
}

#[test]
fn ack_never_moves_back() {
    let tmp = tempfile::tempdir().unwrap();
    let w = WriterLock::take(tmp.path()).unwrap();
    let store = TextStore::open(tmp.path(), "jordi").unwrap();
    appended(&store, &w, 5);
    assert_eq!(store.bookmark("crystalline").unwrap(), None);
    assert_eq!(store.ack("crystalline", 4).unwrap().seq, 4);
    assert_eq!(store.ack("crystalline", 2).unwrap().seq, 4);
    assert_eq!(store.bookmark("crystalline").unwrap().unwrap().seq, 4);
    // Never past the log's end, so no future line is skipped.
    assert_eq!(store.ack("crystalline", 99).unwrap().seq, 5);
    assert!(store.bookmark("crystalline").unwrap().unwrap().updated_at > 0);
    assert!(store.dir().join(".lock").is_file());

    store.ack("agent-2", 1).unwrap();
    let names: Vec<String> = store
        .consumers()
        .unwrap()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(
        names,
        vec!["agent-2".to_string(), "crystalline".to_string()]
    );
    assert!(store.remove_consumer("agent-2").unwrap());
    assert!(!store.remove_consumer("agent-2").unwrap());
    assert_eq!(store.bookmark("agent-2").unwrap(), None);

    assert!(store.ack("../x", 1).is_err());
    assert!(store.bookmark("a/b").is_err());
}

#[test]
fn a_broken_bookmark_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let w = WriterLock::take(tmp.path()).unwrap();
    let store = TextStore::open(tmp.path(), "jordi").unwrap();
    appended(&store, &w, 3);
    let file = store.dir().join("consumers/crystalline.json");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    for broken in ["{not json", ""] {
        std::fs::write(&file, broken).unwrap();
        assert!(store.bookmark("crystalline").is_err(), "{broken:?}");
        assert!(store.ack("crystalline", 2).is_err(), "{broken:?}");
        assert!(store.consumers().is_err(), "{broken:?}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), broken);
    }
}

#[test]
fn read_log_stops_at_up_to() {
    let tmp = tempfile::tempdir().unwrap();
    let w = WriterLock::take(tmp.path()).unwrap();
    let store = TextStore::open(tmp.path(), "jordi").unwrap();
    appended(&store, &w, 5);
    assert_eq!(seqs(&store, 1, Some(3)), vec![2, 3]);
    assert_eq!(seqs(&store, 3, None), vec![4, 5]);
    assert_eq!(seqs(&store, 5, None), Vec::<u64>::new());
    let mut calls = 0;
    let err = store.read_log(0, None, &mut |_| {
        calls += 1;
        Err(crate::Error::WriterBusy)
    });
    assert!(err.is_err());
    assert_eq!(calls, 1, "an error of the callback stops the read");
}

#[test]
fn a_broken_log_line_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let w = WriterLock::take(tmp.path()).unwrap();
    let store = TextStore::open(tmp.path(), "jordi").unwrap();
    appended(&store, &w, 1);
    let log = store.dir().join("log.jsonl");
    let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
    std::io::Write::write_all(&mut f, b"garbage\n").unwrap();
    let err = store.read_log(0, None, &mut |_| Ok(())).unwrap_err();
    assert!(matches!(err, crate::Error::Corrupt { .. }), "{err:?}");
}

#[test]
fn note_keys_name_files_and_nothing_else() {
    let tmp = tempfile::tempdir().unwrap();
    let w = WriterLock::take(tmp.path()).unwrap();
    let store = TextStore::open(tmp.path(), "jordi").unwrap();
    let md5 = "0123456789abcdef0123456789abcdef";
    let key = format!("path:{md5}");
    let state = NoteState {
        key: key.clone(),
        path: "NOTE/Note/x.note".into(),
        ..NoteState::default()
    };
    store.save_note(&w, &state).unwrap();
    assert!(store.dir().join(format!("notes/path-{md5}.json")).is_file());
    assert_eq!(store.note(&key).unwrap(), Some(state.clone()));
    store.save_note(&w, &first_state()).unwrap();
    // A leftover temp file of an atomic write is not a note.
    std::fs::write(store.dir().join("notes/.x.json.1.2.tmp"), "{").unwrap();
    let keys: Vec<String> = store.notes().unwrap().into_iter().map(|n| n.key).collect();
    assert_eq!(keys, vec![KEY.to_string(), key]);
    assert_eq!(store.note(OTHER).unwrap(), None);

    for bad in ["../x", "path:xyz", "a/b", ""] {
        assert!(store.note(bad).is_err(), "{bad}");
        let bad_state = NoteState {
            key: bad.into(),
            ..NoteState::default()
        };
        assert!(store.save_note(&w, &bad_state).is_err(), "{bad}");
    }
}

#[test]
fn region_missed_adds_up_and_survives_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let w = WriterLock::take(tmp.path()).unwrap();
    let store = TextStore::open(tmp.path(), "jordi").unwrap();
    assert_eq!(store.stats().unwrap(), Stats::default());
    store.add_region_missed(&w, 2).unwrap();
    assert_eq!(store.add_region_missed(&w, 3).unwrap().region_missed, 5);
    let store = TextStore::open(tmp.path(), "jordi").unwrap();
    assert_eq!(store.stats().unwrap().region_missed, 5);
    assert!(store.dir().join("stats.json").is_file());
}

#[test]
fn a_bad_slug_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    for bad in ["", "..", "a/b", "."] {
        assert!(TextStore::open(tmp.path(), bad).is_err(), "{bad:?}");
    }
}

#[test]
fn repair_keeps_a_comeback_without_a_line() {
    let tmp = tempfile::tempdir().unwrap();
    let w = WriterLock::take(tmp.path()).unwrap();
    let store = TextStore::open(tmp.path(), "jordi").unwrap();
    store
        .append(
            &w,
            &mut [page_line(
                KEY,
                "P1",
                LineChange::New,
                vec![block("b1", "hello")],
            )],
        )
        .unwrap();
    store.save_note(&w, &first_state()).unwrap();
    store
        .append(&w, &mut [LogLine::new(KEY, LineChange::NoteDeleted)])
        .unwrap();
    let mut gone = first_state();
    gone.deleted = true;
    store.save_note(&w, &gone).unwrap();
    // The note comes back with the same text: no line (spec 2.2), only the state.
    store.save_note(&w, &first_state()).unwrap();
    store.repair(&w).unwrap();
    assert!(!store.note(KEY).unwrap().unwrap().deleted);
}

#[test]
fn repair_keeps_a_position_that_changed_without_a_line() {
    let tmp = tempfile::tempdir().unwrap();
    let w = WriterLock::take(tmp.path()).unwrap();
    let store = TextStore::open(tmp.path(), "jordi").unwrap();
    store
        .append(
            &w,
            &mut [page_line(
                KEY,
                "P1",
                LineChange::New,
                vec![block("b1", "hello")],
            )],
        )
        .unwrap();
    let mut moved = page_line(KEY, "P1", LineChange::Moved, vec![]);
    moved.position = Some(1);
    store.append(&w, &mut [moved]).unwrap();
    let mut state = first_state();
    state.pages[0].position = 1;
    store.save_note(&w, &state).unwrap();
    // Another page before it went away: P1 is at 0 again, with no line of its own.
    state.pages[0].position = 0;
    store.save_note(&w, &state).unwrap();
    store.repair(&w).unwrap();
    assert_eq!(store.note(KEY).unwrap().unwrap().pages[0].position, 0);
}
