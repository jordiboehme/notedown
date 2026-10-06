//! Text boxes, digest references and hidden source strokes against
//! docs/note-format.md 4.3, 7.4, 9.3 and 11.3a.
//!
//! The only sample with a text box is the private bike-digest file: its tests
//! assert structure, counts and the values the spec publishes, never the
//! recognized text or the digest id, and skip when the file is missing. The
//! other tests use synthetic records built from the 9.3 layout.

use std::path::PathBuf;

use base64::Engine;
use notedown_note::{
    NoteError, Notebook, RecordKind, StrokeStatus, VersionPolicy, decode_text_boxes,
    decode_totalpath,
};

const PAGE: (u32, u32) = (1404, 1872);

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

// ---------------------------------------------------------------- record builder

#[derive(Clone)]
struct Rec {
    tool: u32,
    color: u32,
    class: i32,
    kind: &'static str,
    status: i32,
    operation: i32,
    id: u32,
    refs: Vec<u32>,
    points: Vec<(u32, u32)>,
    ring: Vec<(f32, f32)>,
    strings: [Vec<u8>; 3],
}

impl Rec {
    /// An ink stroke (9.3: tool 10, class 5000, kind `others`).
    fn ink(id: u32, status: i32) -> Rec {
        Rec {
            tool: 10,
            color: 0,
            class: 5000,
            kind: "others",
            status,
            operation: 0,
            id,
            refs: vec![],
            points: vec![(1404, 10347), (1500, 10300), (1600, 10200)],
            ring: vec![(10.0, 20.0), (30.0, 20.0), (30.0, 40.0), (10.0, 20.0)],
            strings: [b"none".to_vec(), b"none".to_vec(), vec![]],
        }
    }

    /// A text box record (7.4): class 0, kind `0000`, operation 52.
    fn text_box(id: u32, items: &[String], trails: Vec<u8>) -> Rec {
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut first: Vec<u8> = Vec::new();
        for item in items {
            first.extend_from_slice(b64.encode(item).as_bytes());
            first.push(b',');
        }
        Rec {
            tool: 0,
            color: 0,
            class: 0,
            kind: "0000",
            status: 0,
            operation: 52,
            id,
            refs: vec![],
            points: vec![(0, 0); 5],
            ring: vec![
                (256.0, 883.0),
                (526.0, 883.0),
                (526.0, 796.0),
                (256.0, 796.0),
                (256.0, 883.0),
            ],
            strings: [first, b"none".to_vec(), trails],
        }
    }

    /// The group record (7.4): tool 4, class 5000, kind `others`, operation 52.
    fn group(id: u32, refs: &[u32]) -> Rec {
        Rec {
            tool: 4,
            color: 644_497_065,
            class: 5000,
            kind: "others",
            status: 0,
            operation: 52,
            id,
            refs: refs.to_vec(),
            points: vec![],
            ring: vec![],
            strings: [b"none".to_vec(), b"none".to_vec(), vec![]],
        }
    }

    fn bytes(&self) -> Vec<u8> {
        let mut o: Vec<u8> = Vec::new();
        let u = |o: &mut Vec<u8>, v: u32| o.extend_from_slice(&v.to_le_bytes());
        let i = |o: &mut Vec<u8>, v: i32| o.extend_from_slice(&v.to_le_bytes());
        let name = |o: &mut Vec<u8>, s: &str| {
            let mut field = [0u8; 52];
            field[..s.len()].copy_from_slice(s.as_bytes());
            o.extend_from_slice(&field);
        };
        // A. head
        u(&mut o, self.tool);
        u(&mut o, self.color);
        u(&mut o, 400);
        for v in [10, 0, 32] {
            u(&mut o, v);
        }
        i(&mut o, -1);
        u(&mut o, 3); // page number
        u(&mut o, 0);
        u(&mut o, 0);
        i(&mut o, self.class);
        u(&mut o, 0);
        name(&mut o, self.kind);
        for v in [0, 0, 0, 0, 0, 0, 2] {
            i(&mut o, v); // bbox (6) and offset 124
        }
        u(&mut o, 15819);
        u(&mut o, 11864);
        name(&mut o, "superNoteNote");
        u(&mut o, 1);
        for _ in 0..4 {
            u(&mut o, 0);
        }
        u(&mut o, 0); // disable areas
        assert_eq!(o.len(), 212);
        // B. to E.
        let n = self.points.len() as u32;
        u(&mut o, n);
        for &(a, b) in &self.points {
            u(&mut o, a);
            u(&mut o, b);
        }
        u(&mut o, n);
        for _ in 0..n {
            o.extend_from_slice(&1000u16.to_le_bytes());
        }
        u(&mut o, n);
        for _ in 0..n {
            o.extend_from_slice(&[0, 0, 0, 0]);
        }
        u(&mut o, n);
        o.extend(std::iter::repeat_n(1u8, n as usize));
        // F.
        u(&mut o, 0);
        u(&mut o, 0);
        // G. to I.
        i(&mut o, self.status);
        i(&mut o, self.operation);
        u(&mut o, self.id);
        // J.
        u(&mut o, 0);
        u(&mut o, 0);
        for v in [0, 0, 1, 1, 0, 0, 1, 1] {
            i(&mut o, v);
        }
        // K., L.
        u(&mut o, self.refs.len() as u32);
        for &r in &self.refs {
            u(&mut o, r);
        }
        o.extend_from_slice(&[0; 9]);
        o.push(1);
        // M., N.
        if self.ring.is_empty() {
            u(&mut o, 0);
            u(&mut o, 0);
        } else {
            u(&mut o, 1);
            u(&mut o, self.ring.len() as u32);
            for &(x, y) in &self.ring {
                o.extend_from_slice(&x.to_le_bytes());
                o.extend_from_slice(&y.to_le_bytes());
            }
            u(&mut o, 1);
            for v in [-1, -1, -1, -1] {
                i(&mut o, v);
            }
        }
        // O.
        i(&mut o, -1);
        o.extend_from_slice(&1.0f64.to_le_bytes());
        o.push(0);
        i(&mut o, 0);
        u(&mut o, 0);
        u(&mut o, 1404);
        u(&mut o, 1872);
        u(&mut o, 0);
        o.push(0);
        // P.
        for s in &self.strings {
            u(&mut o, s.len() as u32);
            o.extend_from_slice(s);
        }
        u(&mut o, 3);
        u(&mut o, 0);
        o.extend_from_slice(&[0; 4]);
        o
    }
}

/// A TOTALPATH block (9.1).
fn totalpath(records: &[Rec]) -> Vec<u8> {
    let mut o = (records.len() as u32).to_le_bytes().to_vec();
    for r in records {
        let b = r.bytes();
        o.extend_from_slice(&(b.len() as u32).to_le_bytes());
        o.extend_from_slice(&b);
    }
    o
}

/// The `TEXTBOX_TRAILS` payload (7.4): magic, count, 1000, 4, then count x
/// (length + record).
fn trails(copies: &[Rec]) -> Vec<u8> {
    let mut o = b"TEXTBOX_TRAILS".to_vec();
    o.extend_from_slice(&(copies.len() as u32).to_le_bytes());
    o.extend_from_slice(&1000u32.to_le_bytes());
    o.extend_from_slice(&4u32.to_le_bytes());
    for c in copies {
        let b = c.bytes();
        o.extend_from_slice(&(b.len() as u32).to_le_bytes());
        o.extend_from_slice(&b);
    }
    o
}

fn digest_json() -> String {
    r#"{"digest_source":"1","digest_id":"0123456789abcdef0123456789abcdef","digest_time":"1791270408460"}"#
        .to_owned()
}

/// The 18 items before the digest JSON (7.4), 19 with it.
fn items(text: &str, with_digest: bool) -> Vec<String> {
    let mut v: Vec<String> = [
        "0",
        "0",
        "0",
        "20261006090648924962",
        "256,796,271,88",
        "256,796,271,88",
        "none",
        "none",
        "none",
        "0",
        "48.000000",
        "/system/fonts/DroidSansFallbackFull.ttf",
        text,
        "4,0,0,0.000000,0.000000,1.000000,0,1,0,0,0,0,0,3,0,255,255,0",
        "63",
        "1",
        "1",
        "0",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    if with_digest {
        v.push(digest_json());
    }
    v
}

/// Source strokes 2 and 3 hidden (-5), a text box (id 5) with the embedded copies
/// (status 0) and the group record (id 6).
fn page_records(with_digest: bool) -> Vec<Rec> {
    let copies = [Rec::ink(2, 0), Rec::ink(3, 0)];
    vec![
        Rec::ink(1, 0),
        Rec::ink(2, -5),
        Rec::ink(3, -5),
        Rec::text_box(
            5,
            &items("Hello, \"world\" \u{e9}", with_digest),
            trails(&copies),
        ),
        Rec::group(6, &[2, 3, 5]),
    ]
}

// ---------------------------------------------------------------- synthetic tests

#[test]
fn text_box_with_digest_and_sources() {
    let records = decode_totalpath(&totalpath(&page_records(true)), PAGE).unwrap();
    let boxes = decode_text_boxes(&records, 4).unwrap();
    assert_eq!(boxes.len(), 1);
    let b = &boxes[0];
    assert_eq!((b.page, b.record_index, b.id), (4, 3, 5));
    assert_eq!(b.rect, [256, 796, 271, 88]);
    assert_eq!(b.text, "Hello, \"world\" \u{e9}");
    assert_eq!(b.font_size, Some(48.0));
    assert_eq!(
        b.font_path.as_deref(),
        Some("/system/fonts/DroidSansFallbackFull.ttf")
    );
    assert_eq!(b.created.as_deref(), Some("20261006090648924962"));
    assert_eq!(
        b.style_raw,
        "4,0,0,0.000000,0.000000,1.000000,0,1,0,0,0,0,0,3,0,255,255,0"
    );
    assert_eq!(b.flags_raw, ["63", "1", "1", "0"]);
    let d = b.digest.as_ref().expect("digest");
    assert_eq!(d.id, "0123456789abcdef0123456789abcdef");
    assert_eq!(d.time_ms, Some(1_791_270_408_460));
    assert_eq!(d.source.as_deref(), Some("1"));
    assert_eq!(b.source_stroke_ids, [2, 3]);
    assert_eq!(b.source_strokes.len(), 2);
    assert_eq!(b.source_strokes[0].id, 2);
    assert_eq!(b.source_strokes[1].points.len(), 3);
    assert!(b.source_strokes.iter().all(|s| s.status_code == 0));
    // kinds and statuses of the page records
    assert_eq!(records[1].status, StrokeStatus::ReplacedByTextBox);
    assert_eq!(records[3].kind(), RecordKind::TextBox);
    assert_eq!(records[4].kind(), RecordKind::Group);
}

#[test]
fn text_box_without_digest_json() {
    let records = decode_totalpath(&totalpath(&page_records(false)), PAGE).unwrap();
    let boxes = decode_text_boxes(&records, 0).unwrap();
    assert_eq!(boxes.len(), 1);
    assert!(boxes[0].digest.is_none());
    assert_eq!(boxes[0].text, "Hello, \"world\" \u{e9}");
    assert_eq!(boxes[0].flags_raw, ["63", "1", "1", "0"]);
    assert_eq!(boxes[0].source_strokes.len(), 2);
}

#[test]
fn plain_typed_text_box_has_no_sources() {
    let tb = Rec::text_box(1, &items("typed", false), vec![]);
    let records = decode_totalpath(&totalpath(&[tb]), PAGE).unwrap();
    let boxes = decode_text_boxes(&records, 0).unwrap();
    assert_eq!(boxes.len(), 1);
    assert!(boxes[0].source_stroke_ids.is_empty());
    assert!(boxes[0].source_strokes.is_empty());
}

#[test]
fn link_boxes_are_not_text_boxes() {
    let mut link = Rec::text_box(1, &items("x", false), vec![]);
    link.operation = 500;
    let records = decode_totalpath(&totalpath(&[link]), PAGE).unwrap();
    assert_eq!(records[0].kind(), RecordKind::LinkBox);
    assert!(decode_text_boxes(&records, 0).unwrap().is_empty());
}

fn decode_err(payload: Vec<u8>) -> NoteError {
    let tb = Rec::text_box(1, &items("x", true), payload);
    let records = decode_totalpath(&totalpath(&[tb]), PAGE).unwrap();
    decode_text_boxes(&records, 0).unwrap_err()
}

#[test]
fn truncated_trails_are_a_decode_error() {
    let full = trails(&[Rec::ink(2, 0), Rec::ink(3, 0)]);
    for cut in [full.len() - 1, full.len() / 2, 20, 14] {
        let e = decode_err(full[..cut].to_vec());
        assert!(matches!(e, NoteError::Decode(_)), "cut {cut}: {e:?}");
    }
}

#[test]
fn huge_trail_count_is_a_decode_error() {
    let mut payload = trails(&[Rec::ink(2, 0)]);
    payload[14..18].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(decode_err(payload), NoteError::Decode(_)));
}

#[test]
fn bad_embedded_record_length_is_a_decode_error() {
    let mut payload = trails(&[Rec::ink(2, 0)]);
    payload[26..30].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(decode_err(payload), NoteError::Decode(_)));
}

#[test]
fn too_few_items_are_a_decode_error() {
    let tb = Rec::text_box(1, &items("x", false)[..8], vec![]);
    let records = decode_totalpath(&totalpath(&[tb]), PAGE).unwrap();
    assert!(matches!(
        decode_text_boxes(&records, 0),
        Err(NoteError::Decode(_))
    ));
}

// ---------------------------------------------------------------- synthetic notebook

struct Builder {
    data: Vec<u8>,
}

impl Builder {
    fn block(&mut self, payload: &[u8]) -> u64 {
        let address = self.data.len() as u64;
        self.data
            .extend_from_slice(&(payload.len() as u32).to_le_bytes());
        self.data.extend_from_slice(payload);
        address
    }

    fn meta(&mut self, items: &[(&str, String)]) -> u64 {
        let text: String = items.iter().map(|(k, v)| format!("<{k}:{v}>")).collect();
        self.block(text.as_bytes())
    }
}

/// Two pages: the first with a text box and the `PAGETEXTBOX`/`DISABLE` keys, the
/// second (an old-style page) without them.
fn notebook(disable: &str) -> Notebook {
    let mut b = Builder {
        data: b"noteSN_FILE_VER_20260016".to_vec(),
    };
    let header = b.meta(&[
        ("FILE_TYPE", "NOTE".to_owned()),
        ("APPLY_EQUIPMENT", "N6".to_owned()),
        ("FILE_ID", "F20250102030405060708abcdefABCDEF".to_owned()),
        ("FILE_RECOGN_TYPE", "0".to_owned()),
    ]);
    let path = b.block(&totalpath(&page_records(true)));
    let page1 = b.meta(&[
        ("PAGESTYLE", "style_white".to_owned()),
        ("TOTALPATH", path.to_string()),
        ("PAGEID", "P20250102030405070000abcdefABCDEF".to_owned()),
        ("PAGETEXTBOX", "1".to_owned()),
        ("DISABLE", disable.to_owned()),
    ]);
    let page2 = b.meta(&[
        ("PAGESTYLE", "style_white".to_owned()),
        ("PAGEID", "P20250102030405070001abcdefABCDEF".to_owned()),
    ]);
    let footer = b.meta(&[
        ("PAGE1", page1.to_string()),
        ("PAGE2", page2.to_string()),
        ("FILE_FEATURE", header.to_string()),
    ]);
    b.data.extend_from_slice(b"tail");
    b.data.extend_from_slice(&(footer as u32).to_le_bytes());
    Notebook::from_bytes(b.data, VersionPolicy::Strict).unwrap()
}

#[test]
fn notebook_text_boxes_and_hidden_strokes() {
    let nb = notebook("256,796,271,88|");
    let boxes = nb.text_boxes(0).unwrap();
    assert_eq!(boxes.len(), 1);
    assert_eq!(boxes[0].page, 0);
    assert_eq!(boxes[0].source_stroke_ids, [2, 3]);
    // ink_polylines: only the live stroke; -5 strokes, the text box and the
    // group record are not drawn
    assert_eq!(nb.ink_polylines(0).unwrap().len(), 1);
    // an old-style page without TOTALPATH and the page keys
    assert!(nb.text_boxes(1).unwrap().is_empty());
    assert!(nb.text_boxes(9).is_err());
    let p = nb.page(1).unwrap();
    assert_eq!(p.text_box_flag, None);
    assert!(p.disabled_rects.is_empty());
}

#[test]
fn disabled_rects_parse() {
    let nb = notebook("256,796,271,88|");
    let p = nb.page(0).unwrap();
    assert_eq!(p.text_box_flag, Some(true));
    assert_eq!(p.disabled_rects, vec![[256, 796, 271, 88]]);
    assert!(nb.warnings.is_empty(), "{:?}", nb.warnings);

    let nb = notebook("none");
    assert!(nb.page(0).unwrap().disabled_rects.is_empty());
    assert!(nb.warnings.is_empty());

    let nb = notebook("1,2,3,4|5,6,x,8|9,10,11,12|");
    assert_eq!(
        nb.page(0).unwrap().disabled_rects,
        vec![[1, 2, 3, 4], [9, 10, 11, 12]]
    );
    assert_eq!(nb.warnings.len(), 1, "{:?}", nb.warnings);
    assert_eq!(
        nb.page(0).unwrap().meta.get("DISABLE"),
        Some("1,2,3,4|5,6,x,8|9,10,11,12|")
    );
}

// ---------------------------------------------------------------- local samples

fn local(name: &str) -> Option<Notebook> {
    let path = root().join(format!("tests/fixtures/local/{name}.note"));
    if !path.exists() {
        eprintln!("skipping: local fixture {} is missing", path.display());
        return None;
    }
    Some(Notebook::open(&path, VersionPolicy::Strict).unwrap())
}

#[test]
fn bike_digest_page_3_text_box() {
    let Some(nb) = local("bike-digest") else {
        return;
    };
    assert!(nb.text_boxes(0).unwrap().is_empty());
    assert!(nb.text_boxes(1).unwrap().is_empty());
    let boxes = nb.text_boxes(2).unwrap();
    assert_eq!(boxes.len(), 1);
    let b = &boxes[0];
    // docs/note-format.md 7.4, 11.3a
    assert_eq!(b.rect, [256, 796, 271, 88]);
    assert_eq!(b.id, 61);
    assert_eq!(b.record_index, 60);
    assert_eq!(b.source_stroke_ids, (53..=59).collect::<Vec<u32>>());
    assert_eq!(b.source_strokes.len(), 7);
    assert_eq!(b.font_size, Some(48.0));
    assert!(!b.text.is_empty());
    assert!(b.digest.is_some());
    let records = nb.strokes(2).unwrap();
    assert_eq!(records[b.record_index].len, 15609);
    assert_eq!(records[b.record_index].kind(), RecordKind::TextBox);
    let payload = &records[b.record_index].trailer.as_ref().unwrap().strings[2];
    assert_eq!(payload.len(), 14667);
    assert_eq!(records[61].id, 62);
    assert_eq!(records[61].kind(), RecordKind::Group);
    // hidden handwriting is not drawn: 64 records, 52 old ink + 1 erased tap are
    // not all live, the 7 source strokes are excluded
    let hidden = records
        .iter()
        .filter(|r| r.status == StrokeStatus::ReplacedByTextBox)
        .count();
    assert_eq!(hidden, 7);
    let live = records.iter().filter(|r| r.is_live_ink()).count();
    assert_eq!(nb.ink_polylines(2).unwrap().len(), live);
    let page = nb.page(2).unwrap();
    assert_eq!(page.text_box_flag, Some(true));
    assert_eq!(page.disabled_rects, vec![[256, 796, 271, 88]]);
    // the embedded copies are the source strokes except for the status
    for (copy, id) in b.source_strokes.iter().zip(&b.source_stroke_ids) {
        assert_eq!(copy.id, *id);
        assert_eq!(copy.status_code, 0);
        let orig = records.iter().find(|r| r.id == *id).unwrap();
        assert_eq!(copy.points, orig.points);
    }
}

#[test]
fn samples_without_text_boxes() {
    for name in ["test1", "test2"] {
        let nb = Notebook::open(
            &root().join(format!("tests/fixtures/notes/{name}.note")),
            VersionPolicy::Strict,
        )
        .unwrap();
        for page in 0..nb.pages.len() {
            assert!(nb.text_boxes(page).unwrap().is_empty());
            assert!(nb.page(page).unwrap().disabled_rects.is_empty());
        }
    }
}
