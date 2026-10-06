//! Stroke records (TOTALPATH) and recognition data against docs/note-format.md
//! 8, 9 and 11.
//!
//! The local fixtures (bike, content) are private: only counts are asserted for
//! them, and every test skips with a note when they are missing. Recognized text
//! is never asserted or printed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use base64::Engine;
use notedown_note::{
    NoteError, Notebook, RecordKind, RenderOptions, StrokeStatus, VersionPolicy, decode_ink_bink,
    decode_recogn_file, decode_recogn_text, decode_totalpath,
};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn open(path: PathBuf) -> Notebook {
    match Notebook::open(&path, VersionPolicy::Strict) {
        Ok(nb) => nb,
        Err(e) => panic!("cannot open {}: {e}", path.display()),
    }
}

fn t1() -> Notebook {
    open(root().join("tests/fixtures/notes/test1.note"))
}

fn t2() -> Notebook {
    open(root().join("tests/fixtures/notes/test2.note"))
}

fn local(name: &str) -> Option<Notebook> {
    let path = root().join(format!("tests/fixtures/local/{name}.note"));
    if path.exists() {
        Some(open(path))
    } else {
        eprintln!("skipping: local fixture {} is missing", path.display());
        None
    }
}

/// Counts per page: (records, live ink, erased ink, eraser, lasso, link box).
fn census(nb: &Notebook, page: usize) -> (usize, usize, usize, usize, usize, usize) {
    let records = nb.strokes(page).expect("strokes");
    let mut c = (records.len(), 0, 0, 0, 0, 0);
    for r in &records {
        match (r.kind(), r.status) {
            (RecordKind::Ink, StrokeStatus::Live) => c.1 += 1,
            (RecordKind::Ink, StrokeStatus::Erased) => c.2 += 1,
            (RecordKind::Eraser, _) => c.3 += 1,
            (RecordKind::Lasso, _) => c.4 += 1,
            (RecordKind::LinkBox, _) => c.5 += 1,
            other => panic!("unexpected record {other:?}"),
        }
    }
    c
}

#[test]
fn t1_record_counts_and_first_stroke() {
    let nb = t1();
    // docs/note-format.md 11.1: 72 records, all (tool 10, class 5000, status 0)
    assert_eq!(census(&nb, 0), (72, 72, 0, 0, 0, 0));
    let records = nb.strokes(0).expect("strokes");
    for (i, r) in records.iter().enumerate() {
        assert_eq!(r.index, i);
        assert_eq!(r.tool, 10);
        assert_eq!(r.class, 5000);
        assert_eq!(r.status_code, 0);
        assert_eq!(r.page_number, 1);
        assert_eq!(
            r.id,
            u32::try_from(i + 1).expect("id"),
            "ids rise by 1 (9.1)"
        );
        assert_eq!((r.max_a, r.max_b), (15819, 11864));
        assert_eq!(r.thickness, 400);
        assert_eq!(r.kind_name, "others");
        assert_eq!(r.document_kind, "superNoteNote");
        assert_eq!((r.page_width, r.page_height), (1404, 1872));
        assert_eq!(r.points.len(), r.pressure.len());
        assert_eq!(r.points.len(), r.tilt.len());
        assert_eq!(r.points.len(), r.point_flags.len());
        assert_eq!(r.points.len(), r.points_px.len());
        assert!(r.referenced_ids.is_empty());
        let trailer = r.trailer.as_ref().expect("trailer parses");
        assert_eq!(trailer.constant, 3);
        assert_eq!(trailer.fill_direction.len(), 2);
        assert_eq!(
            r.trailer_extra,
            vec![0, 0, 0, 0],
            "20260016 trailer (9.3 P)"
        );
    }
    // docs/note-format.md 11.1, "first stroke"
    let first = &records[0];
    assert_eq!(first.points.len(), 42);
    assert_eq!(first.points[0], (1404, 10347));
    let (x, y) = first.points_px[0];
    assert!(
        (x - 179.5).abs() < 0.05 && (y - 166.1).abs() < 0.05,
        "({x}, {y})"
    );
    let b = first.bbox;
    assert_eq!(
        [b.left, b.top, b.center_x, b.center_y, b.right, b.bottom],
        [165, 158, 173, 194, 180, 229]
    );
    assert_eq!(first.rings.len(), 1);
    assert_eq!(first.rings[0].points.len(), 7);
    assert_eq!(first.rings[0].parent, -1);
}

#[test]
fn t2_record_counts_erased_stroke_and_eraser() {
    let nb = t2();
    // docs/note-format.md 11.2: 82 live ink, 1 erased ink, 1 eraser
    assert_eq!(census(&nb, 0), (84, 82, 1, 1, 0, 0));
    let records = nb.strokes(0).expect("strokes");
    let erased = &records[78];
    assert_eq!(erased.id, 79);
    assert_eq!(erased.status, StrokeStatus::Erased);
    assert_eq!(erased.status_code, -99);
    assert_eq!(erased.points.len(), 290);
    let eraser = &records[79];
    assert_eq!(eraser.id, 80);
    assert_eq!(eraser.kind(), RecordKind::Eraser);
    assert_eq!(eraser.operation, 601);
    assert_eq!(eraser.referenced_ids, vec![79]);
    assert!(eraser.rings.is_empty());
}

/// Every id an eraser references exists and is an erased record (9.3 G, K).
fn check_erasers(nb: &Notebook, page: usize) -> (usize, BTreeSet<u32>) {
    let records = nb.strokes(page).expect("strokes");
    let by_id: BTreeMap<u32, _> = records.iter().map(|r| (r.id, r)).collect();
    let mut erased_refs = BTreeSet::new();
    let mut erasers = 0;
    for r in records.iter().filter(|r| r.kind() == RecordKind::Eraser) {
        erasers += 1;
        assert!(!r.referenced_ids.is_empty());
        for id in &r.referenced_ids {
            let target = by_id.get(id).unwrap_or_else(|| panic!("id {id} missing"));
            assert_eq!(target.status, StrokeStatus::Erased, "id {id}");
            erased_refs.insert(*id);
        }
    }
    let erased: BTreeSet<u32> = records
        .iter()
        .filter(|r| r.status == StrokeStatus::Erased)
        .map(|r| r.id)
        .collect();
    assert_eq!(erased_refs, erased, "erasers list exactly the erased ids");
    (erasers, erased)
}

#[test]
fn erasers_reference_existing_erased_ids() {
    let (erasers, erased) = check_erasers(&t2(), 0);
    assert_eq!(erasers, 1);
    assert_eq!(erased, BTreeSet::from([79]));
    if let Some(content) = local("content") {
        // docs/note-format.md 9.3 G: 8 erased ids listed by 5 erasers
        let (erasers, erased) = check_erasers(&content, 0);
        assert_eq!((erasers, erased.len()), (5, 8));
    }
}

#[test]
fn local_record_counts() {
    if let Some(bike) = local("bike") {
        // docs/note-format.md 11.3; bike is compacted, no erased records (9.7)
        for (page, n) in [(0, 190), (1, 129), (2, 52)] {
            let records = bike.strokes(page).expect("strokes");
            assert_eq!(records.len(), n);
            assert!(records.iter().all(|r| r.status == StrokeStatus::Live));
            let expected = u32::try_from(page + 1).expect("page");
            assert!(records.iter().all(|r| r.page_number == expected));
            assert!(records.iter().all(|r| r.trailer_extra.is_empty()));
        }
        let ids: Vec<u32> = bike
            .strokes(0)
            .expect("strokes")
            .iter()
            .map(|r| r.id)
            .collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
        assert_eq!((ids[0], ids[ids.len() - 1]), (1, 216), "gaps (9.1)");
    }
    if let Some(content) = local("content") {
        // docs/note-format.md 11.4: 53 live ink, 8 erased ink, 5 eraser, 4 lasso, 6 link box
        assert_eq!(census(&content, 0), (76, 53, 8, 5, 4, 6));
        assert_eq!(content.strokes(1).expect("strokes").len(), 48);
    }
}

#[test]
fn link_boxes_match_links() {
    let Some(content) = local("content") else {
        return;
    };
    let records = content.strokes(0).expect("strokes");
    let boxes: Vec<_> = records
        .iter()
        .filter(|r| r.kind() == RecordKind::LinkBox)
        .collect();
    assert_eq!(boxes.len(), 6);
    let ids: BTreeSet<u32> = records.iter().map(|r| r.id).collect();
    let mut exact = 0;
    for b in &boxes {
        assert_eq!(b.operation, 500);
        assert_eq!(b.link_style, 20000000);
        assert!(b.referenced_ids.iter().all(|id| ids.contains(id)));
        // docs/note-format.md 9.6: the one ring is the LINKRECT rectangle
        assert_eq!(b.rings.len(), 1);
        let ring = &b.rings[0].points;
        assert_eq!(ring.len(), 5);
        let min_x = ring.iter().map(|p| p.0).fold(f32::MAX, f32::min);
        let min_y = ring.iter().map(|p| p.1).fold(f32::MAX, f32::min);
        let max_x = ring.iter().map(|p| p.0).fold(f32::MIN, f32::max);
        let max_y = ring.iter().map(|p| p.1).fold(f32::MIN, f32::max);
        let rect = [min_x, min_y, max_x - min_x, max_y - min_y].map(|v| v.round() as u32);
        // Spec 9.6 says exactly equal; that holds for the four older links, the
        // two newer ones have a ring 1 px narrower and lower than LINKRECT.
        assert!(
            content.links.iter().any(|l| l.rect[0] == rect[0]
                && l.rect[1] == rect[1]
                && l.rect[2].abs_diff(rect[2]) <= 1
                && l.rect[3].abs_diff(rect[3]) <= 1),
            "ring {rect:?} matches a LINKRECT"
        );
        exact += usize::from(content.links.iter().any(|l| l.rect == rect));
        let items = b.link_items().expect("link items");
        // 14 items, 19 on newer links; the string ends with a comma (9.6)
        assert!(items.len() == 14 || items.len() == 19, "{}", items.len());
        assert_eq!(items[0], "1", "LINKTYPE");
    }
    assert_eq!(exact, 4);
    // the first link box lists ids 16 to 25 (9.3 K)
    assert_eq!(boxes[0].referenced_ids, (16..=25).collect::<Vec<u32>>());
    // ink polylines leave out link boxes, erased ink, erasers and lasso paths
    assert_eq!(content.ink_polylines(0).expect("polylines").len(), 53);
}

/// Share of live ink points that land on an opaque pixel of the rendered page.
fn share_on_ink(nb: &Notebook, page: usize) -> (usize, usize) {
    let img = nb
        .render_page(page, &RenderOptions::default())
        .expect("render");
    let (w, h) = img.dimensions();
    let mut on = 0;
    let mut total = 0;
    for line in nb.ink_polylines(page).expect("polylines") {
        for (x, y) in line.points {
            let px = (x.round().max(0.0) as u32).min(w - 1);
            let py = (y.round().max(0.0) as u32).min(h - 1);
            total += 1;
            if img.get_pixel(px, py)[3] != 0 {
                on += 1;
            }
        }
    }
    (on, total)
}

#[test]
fn live_points_land_on_ink() {
    // docs/note-format.md 9.2: every live ink point lands on a non-transparent
    // MAINLAYER pixel after rounding.
    let t2 = t2();
    let (on, total) = share_on_ink(&t2, 0);
    eprintln!("t2: {on} of {total} live points on ink");
    assert!(total > 0);
    assert_eq!(on, total);
    let (on, total) = share_on_ink(&t1(), 0);
    eprintln!("t1: {on} of {total} live points on ink");
    assert_eq!(on, total);
    for name in ["bike", "content"] {
        if let Some(nb) = local(name) {
            for page in 0..nb.pages.len() {
                let (on, total) = share_on_ink(&nb, page);
                eprintln!("{name} page {page}: {on} of {total} live points on ink");
                assert_eq!(on, total);
            }
        }
    }

    // the erased stroke has only 9% of its points on ink (9.3 G)
    let img = t2
        .render_page(0, &RenderOptions::default())
        .expect("render");
    let erased = &t2.strokes(0).expect("strokes")[78];
    let on = erased
        .points_px
        .iter()
        .filter(|(x, y)| img.get_pixel(x.round() as u32, y.round() as u32)[3] != 0)
        .count();
    assert!(on * 100 / erased.points_px.len() < 20, "{on}");
}

#[test]
fn ink_polylines_carry_width_and_color() {
    let nb = t2();
    let lines = nb.ink_polylines(0).expect("polylines");
    assert_eq!(lines.len(), 82);
    for l in &lines {
        assert!(!l.points.is_empty());
        assert!(
            (l.width - 4.0).abs() < f32::EPSILON,
            "thickness 400 / 100 (9.5)"
        );
        assert_eq!(l.color, 0);
    }
}

#[test]
fn freshness_indicator_per_sample_page() {
    // docs/note-format.md 9.7: TOTALPATH after the MAINLAYER bitmap in every final
    // state of the samples. Task 1R's Page::vectors_current is the indicator.
    assert_eq!(t1().pages[0].vectors_current, Some(true));
    assert_eq!(t2().pages[0].vectors_current, Some(true));
    for name in ["bike", "content"] {
        if let Some(nb) = local(name) {
            for page in &nb.pages {
                assert_eq!(page.vectors_current, Some(true), "{name} {}", page.index);
            }
        }
    }
}

#[test]
fn page_without_strokes_and_bad_index() {
    let nb = t1();
    assert!(matches!(nb.strokes(1), Err(NoteError::Decode(_))));
    assert!(matches!(nb.ink_polylines(7), Err(NoteError::Decode(_))));
    assert!(matches!(nb.recognition(3), Err(NoteError::Decode(_))));
    // t2 has neither RECOGNFILE nor RECOGNTEXT
    assert!(t2().recognition(0).expect("recognition").is_none());
}

#[test]
fn t1_recognition_archive() {
    let nb = t1();
    let page = &nb.pages[0];
    assert_eq!(page.recogn_text, None, "t1 has RECOGNTEXT:0 (8.2)");
    assert_eq!(page.recogn_text_block, None);
    let rec = nb.recognition(0).expect("recognition").expect("present");
    assert!(rec.text.is_none());
    let archive = rec.archive.as_ref().expect("archive");
    // docs/note-format.md 8.3: 7 entries with these names and sizes
    let names: Vec<String> = archive
        .entries
        .iter()
        .map(|e| match e.name.strip_prefix("pages/") {
            Some(rest) => {
                let file = rest.rsplit('/').next().unwrap_or("");
                format!("pages/<id>/{file}")
            }
            None => e.name.clone(),
        })
        .collect();
    assert_eq!(
        names,
        [
            "rel.json",
            "index.bdom",
            "meta.json",
            "pages/<id>/ink.bink",
            "pages/<id>/style.css",
            "pages/<id>/page.bdom",
            "pages/<id>/meta.json",
        ]
    );
    let sizes: Vec<usize> = archive.entries.iter().map(|e| e.data.len()).collect();
    assert_eq!(sizes, [135, 233, 225, 72435, 20550, 23599, 345]);
    assert!(archive.entries.iter().all(|e| e.error.is_none()));
    assert!(archive.entries[1].data.starts_with(b"BDOM"));

    let ink = archive.ink.as_ref().expect("ink.bink decoded");
    assert_eq!(ink.strokes.len(), 72);
    assert_eq!(ink.params, [1000, 1000, 3]);
    let first = &ink.strokes[0];
    // creation 2026-10-05 20:53:33 UTC (3.1); first stroke 2 s later
    assert_eq!(first.timestamp_us, 1_791_233_615_777_000);
    assert_eq!(
        first.timestamp_us / 1_000_000 / 86_400,
        20_731,
        "2026-10-05 UTC"
    );
    let last = &ink.strokes[71];
    let span = last.timestamp_us - first.timestamp_us;
    assert!(
        (45_000_000..46_000_000).contains(&span),
        "45.4 s (8.3): {span}"
    );
    assert!(
        ink.strokes
            .windows(2)
            .all(|w| w[0].timestamp_us < w[1].timestamp_us)
    );
    assert!(ink.strokes.iter().all(|s| s.duration == 3145));

    // same count and order as TOTALPATH; mm = pixel * 0.0845 (8.3). Spec 8.3
    // says the point counts are identical; in the file 70 of 72 are, and two
    // ink.bink strokes (39 and 63) have one point more (3 versus 2).
    let records = nb.strokes(0).expect("strokes");
    let differing: Vec<usize> = ink
        .strokes
        .iter()
        .zip(&records)
        .enumerate()
        .filter(|(_, (s, r))| s.points_mm.len() != r.points.len())
        .map(|(i, _)| i)
        .collect();
    assert_eq!(differing, [39, 63]);
    for (s, r) in ink.strokes.iter().zip(&records) {
        assert!(s.points_mm.len().abs_diff(r.points.len()) <= 1);
        assert_eq!(s.force.len(), s.points_mm.len());
        assert_eq!(s.sample_index.len(), s.points_mm.len());
        let (mx, my) = s.points_mm[0];
        let (px, py) = r.points_px[0];
        assert!((mx / px - 0.0845).abs() < 0.001, "{}", mx / px);
        assert!((my / py - 0.0845).abs() < 0.001, "{}", my / py);
    }
    assert!(!ink.rest.is_empty(), "element table kept raw");
}

#[test]
fn recogn_text_decodes_synthetic_block() {
    let json = r#"{"type":"Raw Content","elements":[
        {"type":"Text","label":"alpha beta","words":[
            {"label":"alpha","bounding-box":{"x":1.5,"y":2,"width":3,"height":4}},
            {"label":" "},{"label":"beta"}]},
        {"type":"Raw Content"},
        {"type":"Text","label":"gamma","words":[{"label":"gamma"}]}]}"#;
    let block = base64::engine::general_purpose::STANDARD.encode(json);
    let text = decode_recogn_text(block.as_bytes()).expect("decodes");
    assert_eq!(text.elements.len(), 3);
    assert_eq!(text.text, "alpha beta\ngamma");
    let words = &text.elements[0].words;
    assert_eq!(words.len(), 3);
    let bx = words[0].bounding_box.expect("box");
    assert_eq!((bx.x, bx.y, bx.width, bx.height), (1.5, 2.0, 3.0, 4.0));
    assert!(words[1].bounding_box.is_none());

    assert!(matches!(
        decode_recogn_text(b"not base64 !!"),
        Err(NoteError::Decode(_))
    ));
    let not_json = base64::engine::general_purpose::STANDARD.encode("{");
    assert!(matches!(
        decode_recogn_text(not_json.as_bytes()),
        Err(NoteError::Decode(_))
    ));
}

#[test]
fn bad_archives_are_errors() {
    assert!(matches!(
        decode_recogn_file(b"PK\x03\x04 broken"),
        Err(NoteError::Decode(_))
    ));
    assert!(matches!(
        decode_ink_bink(b"BINK"),
        Err(NoteError::Decode(_))
    ));
    assert!(matches!(
        decode_ink_bink(b"XXXX"),
        Err(NoteError::Decode(_))
    ));
}

/// Deterministic pseudo-random numbers (LCG), no `rand` dependency.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

#[test]
fn fuzz_truncated_and_mutated_totalpath() {
    let nb = t2();
    let block = nb.pages[0].totalpath.expect("totalpath");
    let payload = nb.block(block.address).expect("payload").to_vec();
    let size = (nb.width, nb.height);
    assert_eq!(decode_totalpath(&payload, size).expect("full").len(), 84);

    // truncations: every length up to 600, then about 400 stepped lengths
    let mut lengths: Vec<usize> = (0..600).collect();
    lengths.extend((600..payload.len()).step_by(payload.len() / 400));
    for len in lengths {
        match decode_totalpath(&payload[..len], size) {
            Ok(_) => panic!("truncation to {len} must not parse"),
            Err(NoteError::Decode(_)) => {}
            Err(e) => panic!("unexpected error {e}"),
        }
    }

    // mutations: random bytes, and large values in random u32 positions
    let mut rng = Lcg(0x5eed);
    for round in 0..3000 {
        let mut data = payload.clone();
        let flips = 1 + (rng.next() % 8) as usize;
        for _ in 0..flips {
            let at = (rng.next() as usize) % data.len();
            if round % 2 == 0 {
                data[at] = rng.next() as u8;
            } else {
                let at = at.min(data.len() - 4);
                let v = [0xFFFF_FFFFu32, 0x8000_0000, 0x0100_0000, 0][(rng.next() % 4) as usize];
                data[at..at + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        match decode_totalpath(&data, size) {
            Ok(_) | Err(NoteError::Decode(_)) => {}
            Err(e) => panic!("unexpected error {e}"),
        }
    }
}

#[test]
fn fuzz_ink_bink_and_archive() {
    let nb = t1();
    let rec = nb.recognition(0).expect("recognition").expect("present");
    let archive = rec.archive.expect("archive");
    let bink = archive
        .entries
        .iter()
        .find(|e| e.name.ends_with("ink.bink"))
        .expect("ink.bink")
        .data
        .clone();
    let zip = nb
        .block(nb.pages[0].recogn_file.expect("recognfile").address)
        .expect("zip")
        .to_vec();
    for len in (0..bink.len()).step_by(97) {
        let _ = decode_ink_bink(&bink[..len]);
    }
    for len in (0..zip.len()).step_by(211) {
        let _ = decode_recogn_file(&zip[..len]);
    }
    let mut rng = Lcg(42);
    for _ in 0..500 {
        let mut b = bink.clone();
        let at = (rng.next() as usize) % b.len();
        b[at] = rng.next() as u8;
        let _ = decode_ink_bink(&b);
        let mut z = zip.clone();
        let at = (rng.next() as usize) % z.len();
        z[at] = rng.next() as u8;
        let _ = decode_recogn_file(&z);
    }
}

/// A record count that fits `count * 4` bytes but not `count` minimal records
/// (4-byte length + 212-byte head) is rejected before anything is reserved.
#[test]
fn record_count_is_checked_against_the_minimal_record_size() {
    let count: u32 = 100_000;
    let mut data = count.to_le_bytes().to_vec();
    data.resize(4 + 4 * count as usize, 0);
    match decode_totalpath(&data, (1404, 1872)) {
        Err(NoteError::Decode(m)) => assert!(m.contains("record count"), "{m}"),
        other => panic!("expected a record count error, got {other:?}"),
    }
}

/// Builds a ZIP with `ZipWriter`; `deflate` selects the compression method.
fn build_zip(entries: &[(&str, Vec<u8>)], deflate: bool) -> Vec<u8> {
    use std::io::Write;
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let method = if deflate {
        zip::CompressionMethod::Deflated
    } else {
        zip::CompressionMethod::Stored
    };
    let opts = zip::write::SimpleFileOptions::default().compression_method(method);
    for (name, data) in entries {
        w.start_file(*name, opts).expect("start");
        w.write_all(data).expect("write");
    }
    w.finish().expect("finish").into_inner()
}

fn find(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len())
        .position(|w| w == needle)
        .expect("signature")
}

fn le16(b: &[u8], at: usize) -> usize {
    usize::from(u16::from_le_bytes([b[at], b[at + 1]]))
}

#[test]
fn recognfile_total_budget_flags_entries() {
    // one honest 4 MiB entry: above the total budget (1 MiB floor for a block
    // of a few KiB), refused from its declared size
    let big = build_zip(&[("big.bin", vec![0u8; 4 << 20])], true);
    assert!(big.len() < 64 * 1024);
    let archive = decode_recogn_file(&big).expect("archive");
    let e = &archive.entries[0];
    assert!(e.data.is_empty());
    assert!(
        e.error.as_deref().unwrap_or("").contains("budget"),
        "{:?}",
        e.error
    );

    // three 512 KiB entries: the first two fit the 1 MiB budget, the third not
    let half = vec![0u8; 512 << 10];
    let three = build_zip(
        &[("a", half.clone()), ("b", half.clone()), ("c", half)],
        true,
    );
    let archive = decode_recogn_file(&three).expect("archive");
    let errors: Vec<bool> = archive.entries.iter().map(|e| e.error.is_some()).collect();
    assert_eq!(errors, [false, false, true]);
    assert_eq!(archive.entries[0].data.len(), 512 << 10);

    // a lying central directory (declared size 100) cannot pass the budget either
    let mut lying = big.clone();
    let cd = find(&lying, b"PK\x01\x02");
    lying[cd + 24..cd + 28].copy_from_slice(&100u32.to_le_bytes());
    let local = find(&lying, b"PK\x03\x04");
    lying[local + 22..local + 26].copy_from_slice(&100u32.to_le_bytes());
    let archive = decode_recogn_file(&lying).expect("archive");
    let e = &archive.entries[0];
    assert!(e.error.is_some());
    assert!(e.data.len() <= 1 << 20);
}

#[test]
fn recognfile_overlapping_entries_are_rejected() {
    // a second central directory record "b" pointing at the data of "a"
    let mut z = build_zip(&[("a", b"hello overlapping world".to_vec())], false);
    let cd = find(&z, b"PK\x01\x02");
    let len = 46 + le16(&z, cd + 28) + le16(&z, cd + 30) + le16(&z, cd + 32);
    let mut copy = z[cd..cd + len].to_vec();
    copy[46] = b'b';
    z.splice(cd + len..cd + len, copy);
    let eocd = find(&z, b"PK\x05\x06");
    for at in [eocd + 8, eocd + 10] {
        let n = le16(&z, at) + 1;
        z[at..at + 2].copy_from_slice(&u16::try_from(n).expect("n").to_le_bytes());
    }
    let size = u32::from_le_bytes(z[eocd + 12..eocd + 16].try_into().expect("u32"));
    let size = size + u32::try_from(len).expect("len");
    z[eocd + 12..eocd + 16].copy_from_slice(&size.to_le_bytes());
    match decode_recogn_file(&z) {
        Err(NoteError::Decode(m)) => assert!(m.contains("overlap"), "{m}"),
        other => panic!("expected an overlap error, got {other:?}"),
    }
}
