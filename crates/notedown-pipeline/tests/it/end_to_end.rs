//! End to end (spec 9): notes put into a data folder with `notedown_cloud::put`, the worker, a
//! delta for a consumer, `ack`, then one changed page and a delta with only that change. The
//! change is made in the page record, not with new note bytes (see the test). Plus the one test
//! with the real OCR model, which skips when it is missing.

use std::path::Path;

use notedown_note::{Notebook, VersionPolicy};
use notedown_ocr::model::resolve;
use notedown_ocr::{OcrOptions, VisionEngine};
use notedown_pipeline::delta::{Start, delta, manifest};
use notedown_pipeline::model::Change;
use notedown_pipeline::ocr::read_page;
use time::UtcOffset;

use crate::worker::{SLUG, engine_reading, no_settle, setup, with_parts, worker};

const CONSUMER: &str = "crystalline";
const MAX_CHARS: usize = 30_000;

fn bookmark() -> Start {
    Start::Bookmark(CONSUMER.into())
}

#[test]
fn a_sync_then_a_change_gives_only_that_change() {
    let d = setup();
    // The first sync: two notes in two folders.
    let plan = d.put("test1", "NOTE/Note");
    let work = d.put("test2", "NOTE/Note/Work");
    let p = worker(
        &d,
        no_settle(),
        engine_reading("Meeting with Anna on Friday"),
    )
    .run_once(true)
    .unwrap();
    assert_eq!(p.versions, 2);

    // The first import: both notes are new, with their text.
    let store = d.store();
    let first = delta(&store, &bookmark(), None, MAX_CHARS, UtcOffset::UTC).unwrap();
    assert!(!first.more);
    assert_eq!(first.slug, SLUG);
    let mut keys: Vec<&str> = first.notes.iter().map(|n| n.key.as_str()).collect();
    keys.sort_unstable();
    let mut want = vec![plan.as_str(), work.as_str()];
    want.sort_unstable();
    assert_eq!(keys, want);
    for n in &first.notes {
        assert_eq!(n.change, Change::New, "{n:?}");
        assert_eq!(n.pages.len(), 1, "{n:?}");
        assert_eq!(n.pages[0].text_now[0].text, "Meeting with Anna on Friday");
    }
    let b = store.ack(CONSUMER, first.up_to).unwrap();
    assert_eq!(b.seq, first.up_to);

    // Confirmed: nothing is left for the consumer.
    let empty = delta(&store, &bookmark(), None, MAX_CHARS, UtcOffset::UTC).unwrap();
    assert!(empty.notes.is_empty(), "{empty:?}");

    // The change of one page. There is no fixture with two versions of the same note, so no
    // new note bytes go through `put` here: the test edits the page record of test2 directly
    // and gives it a new layer hash. The worker then reads the whole page, as in
    // `worker::a_layer_change_reads_the_whole_page`; the stroke region diff is not used (a new
    // strokes hash over the same file finds no changed stroke, so nothing would be read).
    // test1 is put again with the same bytes, which changes nothing.
    let again = d.put("test1", "NOTE/Note");
    assert_eq!(again, plan);
    d.edit_record(&work, |r| {
        r.pages[0] = with_parts(&r.pages[0], None, Some("fedcba9876543210"), None);
    });
    let p = worker(
        &d,
        no_settle(),
        engine_reading("Meeting with Anne on Friday"),
    )
    .run_once(true)
    .unwrap();
    assert_eq!(p.pages_read, 1, "only the changed page is read");
    assert_eq!(p.versions, 1);

    // The manifest names only that note.
    let m = manifest(&store, &bookmark(), UtcOffset::UTC).unwrap();
    assert_eq!(m.from, first.up_to);
    assert!(!m.more);
    assert_eq!(m.notes.len(), 1, "{m:?}");
    assert_eq!(m.notes[0].key, work);
    assert_eq!(m.notes[0].folder, "NOTE/Note/Work");
    assert_eq!(m.notes[0].change, Change::Edited);
    assert_eq!(m.notes[0].pages, 1);

    // The delta holds only that page and only the edited block.
    let second = delta(&store, &bookmark(), None, MAX_CHARS, UtcOffset::UTC).unwrap();
    assert_eq!(second.from, first.up_to);
    assert_eq!(second.up_to, m.up_to);
    assert_eq!(second.notes.len(), 1, "{second:?}");
    let note = &second.notes[0];
    assert_eq!(note.key, work);
    assert_eq!(note.change, Change::Edited);
    assert_eq!(note.pages.len(), 1);
    let page = &note.pages[0];
    assert_eq!(page.change, Change::Edited);
    assert_eq!(page.signal, ["layers"]);
    assert_eq!(page.text_now.len(), 1);
    assert_eq!(page.text_now[0].text, "Meeting with Anne on Friday");
    assert_eq!(page.what_changed.len(), 1, "{:?}", page.what_changed);
    let block = &page.what_changed[0];
    assert_eq!(block.change, Change::Edited);
    assert!(
        block
            .block
            .starts_with(&format!("notedown://{SLUG}/{work}/")),
        "{}",
        block.block
    );
    assert_eq!(
        block.word_diff.as_deref(),
        Some("Meeting with [-Anna-][+Anne+] on Friday")
    );

    // The same range as a fixed range for that note gives the same answer.
    let fixed = Start::Range {
        from: m.from,
        up_to: m.up_to,
    };
    let only = delta(
        &store,
        &fixed,
        Some(std::slice::from_ref(&work)),
        MAX_CHARS,
        UtcOffset::UTC,
    )
    .unwrap();
    assert_eq!(only.notes, second.notes);

    // After the ack, the consumer is up to date again.
    store.ack(CONSUMER, second.up_to).unwrap();
    let done = delta(&store, &bookmark(), None, MAX_CHARS, UtcOffset::UTC).unwrap();
    assert!(done.notes.is_empty(), "{done:?}");
}

/// The real model reads one fixture page. Skips when the model files are not in the cache
/// (`notedown model pull`), so the normal run needs no model. One page only: about a minute.
#[test]
fn real_model_reads_a_fixture_page() {
    let paths = match resolve(None, None) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("skipping: the OCR model is missing ({e})");
            return;
        }
    };
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/notes/test1.note");
    let nb = Notebook::open(&path, VersionPolicy::AllowUnknown).unwrap();
    let start = std::time::Instant::now();
    let mut engine = VisionEngine::load(&paths, OcrOptions::default()).unwrap();
    let reading = read_page(&nb, 0, &mut engine).unwrap();
    eprintln!(
        "read page 1 of test1.note in {:?}: {:?}",
        start.elapsed(),
        reading.blocks
    );
    assert!(!reading.truncated);
    assert!(!reading.blocks.is_empty());
    assert!(
        reading.blocks.iter().any(|b| !b.text.trim().is_empty()),
        "{:?}",
        reading.blocks
    );
    assert_ne!(reading.model, "fake-engine");
}
