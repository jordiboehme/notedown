use std::collections::HashMap;
use std::path::Path;

use notedown_convert::pipeline::{NoteSource, PageSource};
use notedown_note::{DigestRef, Notebook, VersionPolicy};
use notedown_pipeline::meta::note_meta;
use notedown_pipeline::ocr::read_page;

use crate::fake_engine::{FakeEngine, block, text_block};

fn fixture() -> Notebook {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/notes/test1.note");
    Notebook::open(&path, VersionPolicy::AllowUnknown).unwrap()
}

#[test]
fn a_blank_page_skips_the_engine() {
    let mut nb = fixture();
    nb.pages[0].layers.clear();
    let mut engine = FakeEngine::new(HashMap::new());
    let reading = read_page(&nb, 0, &mut engine).unwrap();
    assert!(reading.blocks.is_empty());
    assert!(!reading.truncated);
    assert_eq!(engine.calls(), 0);
    assert_eq!(reading.model, "fake-engine");
}

#[test]
fn blocks_come_with_grid_boxes() {
    let nb = fixture();
    let png = NoteSource::new(&nb, false).png(0).unwrap();
    let blocks = vec![
        text_block([10, 20, 500, 120], "first"),
        text_block([10, 200, 500, 300], "second"),
        block("Image", Some([1, 2, 3, 4]), "> Sketch: a cup"),
        block("Text", None, "loose"),
    ];
    let mut engine = FakeEngine::new(HashMap::new());
    engine.insert(&png, blocks);
    let reading = read_page(&nb, 0, &mut engine).unwrap();
    assert_eq!(engine.calls(), 1);
    assert_eq!(reading.blocks.len(), 4, "{:?}", reading.blocks);
    assert_eq!(reading.blocks[0].bbox, [10, 20, 500, 120]);
    assert_eq!(reading.blocks[0].text, "first");
    assert_eq!(reading.blocks[0].label, "Text");
    assert_eq!(reading.blocks[1].bbox, [10, 200, 500, 300]);
    // A block without a box gets the whole page.
    assert_eq!(reading.blocks[2].label, "Image");
    assert_eq!(reading.blocks[2].bbox, [1, 2, 3, 4]);
    assert_eq!(reading.blocks[3].bbox, [0, 0, 1000, 1000]);
    assert_eq!(reading.blocks[3].text, "loose");
    assert_eq!(
        (reading.width, reading.height),
        (nb.pages[0].width, nb.pages[0].height)
    );
    assert_eq!(reading.landscape, nb.pages[0].landscape);
}

#[test]
fn a_truncated_page_is_marked() {
    let nb = fixture();
    let mut engine = FakeEngine::new(HashMap::new()).truncated(true);
    let reading = read_page(&nb, 0, &mut engine).unwrap();
    assert!(reading.truncated);
    assert_eq!(reading.blocks.len(), 1);
}

#[test]
fn an_engine_failure_is_an_error() {
    let nb = fixture();
    let mut engine = FakeEngine::new(HashMap::new());
    engine.fail_next(1);
    assert!(read_page(&nb, 0, &mut engine).is_err());
    assert!(read_page(&nb, 0, &mut engine).is_ok());
    assert_eq!(engine.calls(), 2);
}

#[test]
fn note_meta_of_a_fixture() {
    let mut nb = fixture();
    let digests = vec![
        DigestRef {
            id: "d1".into(),
            time_ms: Some(1),
            source: None,
        },
        DigestRef {
            id: "d2".into(),
            time_ms: None,
            source: None,
        },
    ];
    let meta = note_meta(&nb, &digests);
    assert!(meta.keywords.is_empty());
    assert!(meta.links.is_empty());
    assert!(meta.stars.is_empty());
    assert_eq!(meta.digests, ["d1", "d2"]);
    // The fixture has recognition data but no RECOGNTEXT, so there is no device text.
    assert!(meta.recognition.is_empty());

    // Keywords, links and stars come from the notebook.
    let mut keyword = nb_keyword();
    keyword.text = Some("budget".into());
    nb.keywords.push(keyword);
    let mut link = nb_link(notedown_note::LinkKind::File);
    link.target = Some("/Note/Other.note".into());
    link.target_file_id = Some("F2026".into());
    nb.links.push(link);
    let mut web = nb_link(notedown_note::LinkKind::Web);
    web.target = Some("https://example.com".into());
    nb.links.push(web);
    nb.pages[0].five_star = Some("1".into());
    let meta = note_meta(&nb, &[]);
    assert_eq!(meta.keywords, ["budget"]);
    assert_eq!(meta.stars, [0]);
    assert_eq!(meta.links.len(), 2);
    assert_eq!(meta.links[0].target, "/Note/Other.note");
    assert_eq!(meta.links[0].key.as_deref(), Some("F2026"));
    assert_eq!(meta.links[1].key, None);
}

fn nb_keyword() -> notedown_note::Keyword {
    notedown_note::Keyword {
        key: "KEYWORD_x".into(),
        address: 0,
        page: 0,
        position: key_position(),
        keyword_page: None,
        rect: [0; 4],
        rect_original: None,
        seq_no: None,
        len: None,
        text: None,
        site_text: None,
        inline_text: None,
        site: None,
        meta: Default::default(),
    }
}

fn nb_link(kind: notedown_note::LinkKind) -> notedown_note::Link {
    notedown_note::Link {
        key: "LINKO_x".into(),
        address: 0,
        page: 0,
        position: key_position(),
        kind,
        direction: None,
        target: None,
        target_file_id: None,
        target_page_id: None,
        target_page: None,
        rect: [0; 4],
        rect_original: None,
        timestamp: None,
        style: None,
        protocol: None,
        bitmap_block: None,
        bitmap: None,
        meta: Default::default(),
    }
}

fn key_position() -> notedown_note::KeyPosition {
    notedown_note::KeyPosition {
        page: 1,
        top: None,
        left: None,
        height: None,
        width: None,
    }
}
