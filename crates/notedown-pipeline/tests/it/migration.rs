//! The upgrade from Chandra to pagevision (spec addendum, section 4): pages that Chandra read
//! keep their blocks until the page changes, and pagevision blocks compose with them.

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use notedown_ocr::model::ENGINE_LABEL;
use notedown_pipeline::compose::{Composed, NewBlock, compose};
use notedown_pipeline::delta::{Start, delta};
use notedown_pipeline::model::{Block, Change, LineChange, LogLine, NoteState};
use notedown_pipeline::region::{Rect, Region};
use notedown_pipeline::store::{TextStore, WriterLock};
use time::UtcOffset;

use crate::fake_engine::{FakeEngine, text_block};
use crate::worker::{no_settle, setup, worker};

/// The model name notedown 0.1 stored.
const CHANDRA: &str = "chandra-ocr-2 Q4_K_M";
/// Not 1000 x 1000, so a missing grid conversion fails a test.
const PAGE: (u32, u32, bool) = (2000, 3000, false);

fn note_state(d: &crate::worker::Data, key: &str) -> NoteState {
    d.store().note(key).unwrap().unwrap()
}

#[test]
fn chandra_page_is_not_read_again_after_upgrade() {
    let d = setup();
    let key = d.put("test1", "NOTE/Note");
    // notedown 0.1 read the page with Chandra.
    let html = format!(
        "{}{}",
        text_block([100, 100, 900, 200], "Meeting with Anna"),
        r#"<div data-label="Diagram" data-bbox="100 300 900 700"><pre>graph TD; A[Start] --&gt; B[End];</pre></div>"#
    );
    let chandra = FakeEngine::new(HashMap::new())
        .with_default(&html)
        .with_label(CHANDRA);
    worker(&d, no_settle(), chandra).run_once(true).unwrap();
    let before = note_state(&d, &key);
    assert_eq!(before.model.as_deref(), Some(CHANDRA));
    assert!(before.pages[0].blocks.len() >= 2, "{before:?}");

    // After the upgrade: same strokes, the pagevision engine. Nothing is read again.
    let pagevision = FakeEngine::new(HashMap::new())
        .with_default(&text_block([0, 0, 1000, 1000], "read again"))
        .with_label(ENGINE_LABEL);
    let calls = pagevision.calls_handle();
    worker(&d, no_settle(), pagevision).run_once(true).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let after = note_state(&d, &key);
    assert_eq!(after.model.as_deref(), Some(CHANDRA));
    assert_eq!(after.pages.len(), before.pages.len());
    for (a, b) in after.pages.iter().zip(&before.pages) {
        assert_eq!(a.blocks, b.blocks);
        assert_eq!(a.seq, b.seq);
    }
}

fn old(id: &str, label: &str, bbox: [u32; 4], text: &str) -> Block {
    Block {
        id: id.into(),
        label: label.into(),
        bbox,
        text: text.into(),
        replaces: vec![],
    }
}

fn new(label: &str, bbox: [u32; 4], text: &str) -> NewBlock {
    NewBlock {
        label: label.into(),
        bbox,
        text: text.into(),
    }
}

/// A change region from grid boxes, in page pixels.
fn region(grid: &[[u32; 4]]) -> Region {
    Region::Parts(
        grid.iter()
            .map(|g| Rect {
                x0: g[0] as f32 * 2.0,
                y0: g[1] as f32 * 3.0,
                x1: g[2] as f32 * 2.0,
                y1: g[3] as f32 * 3.0,
            })
            .collect(),
    )
}

/// The store invariants of a composed page: unique ids, new ids only from `next_before`
/// on, every `replaces` names an old block that is gone, `replaced` agrees with the blocks,
/// and the blocks are in reading order.
fn assert_consistent(old: &[Block], c: &Composed, next_before: u64, next_after: u64) {
    let mut ids: Vec<&str> = c.blocks.iter().map(|b| b.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), c.blocks.len(), "duplicate ids: {:?}", c.blocks);
    let old_ids: Vec<&str> = old.iter().map(|b| b.id.as_str()).collect();
    for b in &c.blocks {
        if !old_ids.contains(&b.id.as_str()) {
            let n: u64 = b.id[1..].parse().unwrap();
            assert!((next_before..next_after).contains(&n), "{} not new", b.id);
        }
        for r in &b.replaces {
            assert!(old_ids.contains(&r.as_str()), "{r} is no old block");
            assert!(c.blocks.iter().all(|k| &k.id != r), "{r} replaced and kept");
        }
    }
    for (gone, by) in &c.replaced {
        assert!(c.blocks.iter().all(|k| &k.id != gone));
        for id in by {
            let heir = c.blocks.iter().find(|k| &k.id == id).unwrap();
            assert!(heir.replaces.contains(gone));
        }
    }
    let order: Vec<(u32, u32)> = c.blocks.iter().map(|b| (b.bbox[1], b.bbox[0])).collect();
    let mut sorted = order.clone();
    sorted.sort_unstable();
    assert_eq!(order, sorted);
}

#[test]
fn pagevision_blocks_compose_with_chandra_blocks() {
    let chandra_diagram = "```mermaid\ngraph TD\nA[Start] --> B[End]\n```";
    let olds = [
        old("b1", "Text", [100, 50, 900, 150], "Plan for the week"),
        old("b2", "Diagram", [100, 200, 900, 600], chandra_diagram),
        old("b3", "Text", [100, 700, 900, 800], "Buy milk"),
    ];
    // pagevision reads the drawing as a sketch ("Image") at the same place.
    let news = [
        new("Text", [100, 50, 900, 150], "Plan for the week"),
        new(
            "Image",
            [100, 200, 900, 600],
            "> Sketch: A box Start with an arrow to a box End.",
        ),
        new(
            "Text",
            [100, 700, 900, 800],
            "Call the plumber about the sink",
        ),
    ];
    let mut next = 4;
    // Only the strokes under b3 changed.
    let c = compose(
        &olds,
        &news,
        &region(&[[300, 720, 800, 780]]),
        PAGE,
        &mut next,
    );
    assert_consistent(&olds, &c, 4, next);
    let ids: Vec<&str> = c.blocks.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(ids, ["b1", "b2", "b3"], "{:?}", c.blocks);
    // b1 is untouched and kept as Chandra read it.
    assert_eq!(c.blocks[0], olds[0]);
    // b2 is matched across "Diagram" and "Image": one label class, so it keeps its id.
    assert_eq!(c.blocks[1].id, "b2");
    // Its new reading is far from the old Mermaid, so the safety net gives it the sketch.
    assert_eq!(c.blocks[1].label, "Image");
    assert!(
        c.blocks[1].text.starts_with("> Sketch:"),
        "{:?}",
        c.blocks[1]
    );
    assert_eq!(c.region_missed, 1);
    assert!(c.replaced.is_empty(), "{:?}", c.replaced);
    // b3 takes the new reading.
    assert_eq!(c.blocks[2].text, "Call the plumber about the sink");
    assert!(!c.unchanged);
}

#[test]
fn coarse_text_block_replaces_two_old_blocks() {
    // Chandra gave two text blocks; pagevision reads the same lines as one region.
    let olds = [
        old("b1", "Text", [100, 100, 900, 200], "first part"),
        old("b2", "Text", [100, 200, 900, 300], "second part"),
    ];
    let news = [new(
        "Text",
        [100, 100, 900, 300],
        "first part and second part",
    )];
    let mut next = 3;
    let c = compose(
        &olds,
        &news,
        &region(&[[850, 150, 900, 250]]),
        PAGE,
        &mut next,
    );
    assert_consistent(&olds, &c, 3, next);
    assert_eq!(c.blocks.len(), 1);
    assert_eq!(c.blocks[0].id, "b3");
    assert_eq!(c.blocks[0].replaces, ["b1", "b2"]);

    // The change list of the MCP tools shows both old blocks as replaced by b3.
    const KEY: &str = "F20240424175151947577oMie9HNQYnTa";
    let tmp = tempfile::tempdir().unwrap();
    let w = WriterLock::take(tmp.path()).unwrap();
    let store = TextStore::open_writer(tmp.path(), "jordi", &w).unwrap();
    let line = |change, model: &str, blocks: Vec<Block>| {
        let mut l = LogLine::new(KEY, change);
        l.page_id = Some("P1".into());
        l.position = Some(0);
        l.path = Some("Notes/Alpha.note".into());
        l.model = Some(model.into());
        l.blocks = blocks;
        l
    };
    store
        .append(&w, &mut [line(LineChange::New, CHANDRA, olds.to_vec())])
        .unwrap();
    store.ack("c", 1).unwrap();
    store
        .append(
            &w,
            &mut [line(LineChange::Edited, ENGINE_LABEL, c.blocks.clone())],
        )
        .unwrap();
    let d = delta(
        &store,
        &Start::Bookmark("c".into()),
        None,
        30_000,
        UtcOffset::UTC,
    )
    .unwrap();
    let page = &d.notes[0].pages[0];
    let anchor = |b: &str| format!("notedown://jordi/{KEY}/P1#{b}");
    let got: Vec<(String, Change, Vec<String>)> = page
        .what_changed
        .iter()
        .map(|b| (b.block.clone(), b.change, b.replaced_by.clone()))
        .collect();
    assert_eq!(
        got,
        [
            (anchor("b1"), Change::Replaced, vec![anchor("b3")]),
            (anchor("b2"), Change::Replaced, vec![anchor("b3")]),
        ]
    );
}
