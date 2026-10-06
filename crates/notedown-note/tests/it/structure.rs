//! Structure and metadata tests against the facts in docs/note-format.md 11.
//!
//! The local fixtures (bike, content) are private: only structure, addresses and
//! hashes are asserted, plus the one Bike link the brief allows.

use std::path::PathBuf;

use notedown_note::{
    BlockKind, KNOWN_SIGNATURES, LayerSlot, LinkDirection, LinkKind, NoteError, Notebook,
    VersionPolicy,
};
use sha2::{Digest, Sha256};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixture(name: &str) -> PathBuf {
    root().join("tests/fixtures/notes").join(name)
}

fn local_fixture(name: &str) -> Option<PathBuf> {
    let path = root().join("tests/fixtures/local").join(name);
    if path.exists() {
        Some(path)
    } else {
        eprintln!("skipping: local fixture {} is missing", path.display());
        None
    }
}

fn open(path: &std::path::Path) -> Notebook {
    match Notebook::open(path, VersionPolicy::Strict) {
        Ok(nb) => nb,
        Err(e) => panic!("cannot open {}: {e}", path.display()),
    }
}

fn sha256(bytes: &[u8]) -> String {
    const_hex::encode(Sha256::digest(bytes))
}

fn record_count(nb: &Notebook, address: u64) -> u32 {
    let payload = nb.block(address).unwrap();
    u32::from_le_bytes(payload[..4].try_into().unwrap())
}

/// Consecutive duplicates removed.
fn dedup(values: impl IntoIterator<Item = u64>) -> Vec<u64> {
    let mut out: Vec<u64> = Vec::new();
    for v in values {
        if out.last() != Some(&v) {
            out.push(v);
        }
    }
    out
}

// ---------------------------------------------------------------- t1 (11.1)

#[test]
fn t1_envelope_and_header() {
    let nb = open(&fixture("test1.note"));
    assert_eq!(nb.bytes().len(), 186649);
    assert_eq!(nb.file_type, "note");
    assert_eq!(nb.signature, "SN_FILE_VER_20260016");
    assert_eq!(nb.version, Some(20260016));
    assert!(nb.version_known);
    assert_eq!(nb.footer_address, 186563);
    assert_eq!(nb.header_address, Some(3194));
    assert_eq!(
        nb.file_id.as_deref(),
        Some("F20261005225333089589QOPqpMYT9WQV")
    );
    assert_eq!(
        nb.created.map(|t| t.to_string()).as_deref(),
        Some("2026-10-05 22:53:33.089589")
    );
    assert_eq!(nb.equipment.as_deref(), Some("N6"));
    assert!(nb.device_known);
    assert_eq!((nb.width, nb.height), (1404, 1872));
    assert!(nb.realtime_recognition);
    assert_eq!(nb.recognition_language.as_deref(), Some("en_US"));
    assert_eq!(nb.dirty, Some(1));
    assert_eq!(nb.header.get("MODULE_LABEL"), Some("none"));
    assert_eq!(nb.header.get("ANTIALIASING_CONVERT"), Some("2"));
    assert_eq!(nb.header.len(), 20);
    assert_eq!(nb.footer.get("STYLE_style_8mm_ruled_line"), Some("330"));
    assert!(nb.links.is_empty());
    assert!(nb.titles.is_empty());
    assert!(nb.keywords.is_empty());
    assert!(nb.covers.is_empty());
    assert!(nb.warnings.is_empty(), "{:?}", nb.warnings);
}

#[test]
fn t1_page() {
    let nb = open(&fixture("test1.note"));
    assert_eq!(nb.pages.len(), 1);
    let page = &nb.pages[0];
    assert_eq!(page.index, 0);
    assert_eq!(page.address, 185284);
    assert_eq!(
        page.id.as_deref(),
        Some("P20261005225333098827MOB0TMmka3qk")
    );
    assert_eq!(page.style.as_deref(), Some("style_8mm_ruled_line"));
    assert_eq!(page.style_md5, None);
    assert_eq!(page.orientation, Some(1000));
    assert!(!page.landscape);
    assert_eq!((page.width, page.height), (1404, 1872));
    assert_eq!(page.layer_seq, vec!["MAINLAYER", "BGLAYER"]);
    assert_eq!(page.recogn_status, Some(2));
    assert_eq!(page.recogn_file_status, Some(1));
    assert_eq!(page.recogn_text, None);
    assert_eq!(page.page_text_box, Some(0));
    assert_eq!(page.disable, None);
    assert_eq!(page.five_star, None);
    assert_eq!(page.vectors_current, Some(true));

    let info = page.layer_info.as_ref().expect("LAYERINFO");
    assert_eq!(info.len(), 5);
    assert_eq!(info[3].layer_id, Some(0));
    assert_eq!(info[3].name.as_deref(), Some("Main Layer"));
    assert_eq!(info[4].is_background, Some(true));
    assert_eq!(info[0].is_deleted, Some(true));

    assert_eq!(page.layers.len(), 2);
    let main = &page.layers[0];
    assert_eq!(main.slot, LayerSlot::Main);
    assert_eq!(main.name, "MAINLAYER");
    assert_eq!(main.protocol, "RATTA_RLE");
    assert_eq!(main.address, 48971);
    let bitmap = main.bitmap.expect("MAINLAYER bitmap");
    assert_eq!((bitmap.address, bitmap.len), (5037, 43930));
    assert!(main.visible);
    assert_eq!(main.display_name.as_deref(), Some("Main Layer"));
    let bg = &page.layers[1];
    assert_eq!(bg.slot, LayerSlot::Background);
    assert_eq!(bg.name, "BGLAYER");
    assert_eq!(bg.address, 1255);
    let bitmap = bg.bitmap.expect("BGLAYER bitmap");
    assert_eq!((bitmap.address, bitmap.len), (330, 462));
    assert_eq!(bg.display_name.as_deref(), Some("Background Layer"));

    let totalpath = page.totalpath.expect("TOTALPATH");
    assert_eq!((totalpath.address, totalpath.len), (50458, 104841));
    assert_eq!(
        sha256(nb.block(50458).unwrap()),
        "2488505f0e3814f40e679cbf4b50349e2b30158b6fd48bc0bd6cf78d400de162"
    );
    assert_eq!(record_count(&nb, 50458), 72);
    let recogn = page.recogn_file.expect("RECOGNFILE");
    assert_eq!((recogn.address, recogn.len), (156663, 27252));
    assert!(nb.block(156663).unwrap().starts_with(b"PK\x03\x04"));

    assert_eq!(nb.templates.len(), 1);
    assert_eq!(nb.templates[0].name, "style_8mm_ruled_line");
    assert_eq!(nb.templates[0].bitmap.map(|b| b.address), Some(330));
    assert_eq!(nb.templates[0].pages, vec![0]);
    assert_eq!(nb.page(0).map(|p| p.address), Some(185284));
    assert!(nb.page(1).is_none());
}

#[test]
fn t1_history() {
    let nb = open(&fixture("test1.note"));
    assert_eq!(nb.history_error, None);
    assert_eq!(nb.blocks.len(), 25);
    assert_eq!(nb.saves.len(), 8);
    assert_eq!(nb.saves.last().unwrap().footer_address, nb.footer_address);
    let stale_footers = nb
        .saves
        .iter()
        .filter(|s| s.footer_address != nb.footer_address)
        .count();
    assert_eq!(stale_footers, 7);
    // docs/note-format.md 2, "Stale blocks"
    assert_eq!(
        dedup(nb.saves.iter().filter_map(|s| s.header_address)),
        vec![24, 2704, 3194]
    );
    let page1 = nb
        .saves
        .iter()
        .filter_map(|s| s.page_addresses.first().map(|p| p.1));
    assert_eq!(
        dedup(page1),
        vec![1384, 3684, 49103, 155303, 183919, 185284]
    );
    // footer, header, page, 2 layer blocks, 2 bitmaps, TOTALPATH, RECOGNFILE
    let reachable: Vec<u64> = nb
        .blocks
        .iter()
        .filter(|b| b.reachable)
        .map(|b| b.address)
        .collect();
    assert_eq!(
        reachable,
        vec![330, 1255, 3194, 5037, 48971, 50458, 156663, 185284, 186563]
    );
    let footer = nb.blocks.iter().find(|b| b.address == 186563).unwrap();
    assert_eq!(footer.kind, BlockKind::Metadata);
    let bitmap = nb.blocks.iter().find(|b| b.address == 5037).unwrap();
    assert_eq!(bitmap.kind, BlockKind::Binary);
    assert_eq!(
        nb.blocks.iter().map(|b| b.len as u64 + 4).sum::<u64>() + 24 + 8 * 8,
        186649
    );
}

#[test]
fn block_accessor_is_bounds_checked() {
    let nb = open(&fixture("test1.note"));
    assert_eq!(nb.block(5037).unwrap().len(), 43930);
    assert_eq!(
        nb.block(186563).unwrap().len(),
        nb.bytes().len() - 186563 - 4 - 8
    );
    for address in [0, 3, 23, 186649, 186646, 186645, u64::MAX] {
        assert!(
            matches!(nb.block(address), Err(NoteError::Corrupt(_))),
            "address {address}"
        );
    }
}

// ---------------------------------------------------------------- t2 (11.2)

#[test]
fn t2_structure() {
    let nb = open(&fixture("test2.note"));
    assert_eq!(nb.bytes().len(), 240988);
    assert!(nb.warnings.is_empty(), "{:?}", nb.warnings);
    assert_eq!(nb.signature, "SN_FILE_VER_20260016");
    assert_eq!(nb.footer_address, 240904);
    assert_eq!(nb.header_address, Some(24));
    assert_eq!(
        nb.file_id.as_deref(),
        Some("F20261005225535133622IHP5FqtIJoxD")
    );
    assert_eq!(nb.equipment.as_deref(), Some("N6"));
    assert!(!nb.realtime_recognition);
    assert_eq!(nb.recognition_language, None);
    // short header variant (docs/note-format.md 1)
    assert_eq!(nb.header.get("MODULE_LABEL"), None);
    assert_eq!(nb.header.len(), 14);

    assert_eq!(nb.pages.len(), 1);
    let page = &nb.pages[0];
    assert_eq!(page.index, 0);
    assert_eq!(page.address, 239630);
    assert_eq!(
        page.id.as_deref(),
        Some("P20261005225535142460xFi05hBOdWD6")
    );
    assert_eq!(page.style.as_deref(), Some("style_8mm_ruled_line"));
    assert_eq!(page.layer_seq, vec!["MAINLAYER", "BGLAYER"]);
    assert_eq!(page.recogn_status, Some(0));
    assert_eq!(page.recogn_file, None);
    let main = page.layer(LayerSlot::Main).expect("MAINLAYER");
    assert_eq!(main.address, 81084);
    let bitmap = main.bitmap.unwrap();
    assert_eq!((bitmap.address, bitmap.len), (2704, 78376));
    let bg = page.layer(LayerSlot::Background).expect("BGLAYER");
    assert_eq!((bg.address, bg.bitmap.unwrap().address), (1255, 330));
    assert_eq!(
        nb.block(330).unwrap(),
        open(&fixture("test1.note")).block(330).unwrap()
    );
    let totalpath = page.totalpath.unwrap();
    assert_eq!((totalpath.address, totalpath.len), (82569, 157057));
    assert_eq!(
        sha256(nb.block(82569).unwrap()),
        "93bb6a117f2921c09ece7cfa8fa6b407be2ce78412084c09122ad4524c061f97"
    );
    assert_eq!(record_count(&nb, 82569), 84);
    assert_eq!(page.vectors_current, Some(true));

    assert!(nb.links.is_empty() && nb.titles.is_empty() && nb.keywords.is_empty());
    assert_eq!(nb.saves.len(), 3);
    assert_eq!(nb.blocks.len(), 14);
    assert_eq!(nb.history_error, None);
}

// ---------------------------------------------------------------- bike (11.3)

#[test]
fn bike_structure() {
    let Some(path) = local_fixture("bike.note") else {
        return;
    };
    let nb = open(&path);
    assert_eq!(nb.bytes().len(), 1091064);
    assert!(nb.warnings.is_empty(), "{:?}", nb.warnings);
    assert_eq!(nb.signature, "SN_FILE_VER_20230015");
    assert_eq!(nb.equipment.as_deref(), Some("N6"));
    assert_eq!(
        nb.file_id.as_deref(),
        Some("F20240708174639550429bHksqaP2wKQ8")
    );
    assert_eq!(nb.dirty, Some(0));
    assert_eq!(nb.saves.len(), 1);
    assert_eq!(nb.blocks.len(), 19);
    assert!(
        nb.blocks.iter().all(|b| b.reachable),
        "compacted file has no stale blocks"
    );

    let expected = [
        (
            544370,
            1233,
            108918,
            110416,
            190,
            "9ac691b048787dd6a74fc825ea82ea32c7dd72d6947f2a2fe1e26887dcce9522",
        ),
        (
            958869,
            545619,
            83966,
            629852,
            129,
            "00edacc7c17f0d0c5c5655d2ff888caaaf8dede85be7580c2ddc560494fbe1c0",
        ),
        (
            1089668,
            960118,
            23920,
            984305,
            52,
            "0b468183c96857f7a3304b1cd262683d0218587514462e6d7a6b4611a32587e2",
        ),
    ];
    assert_eq!(nb.pages.len(), 3);
    for (i, (page, (addr, bitmap, bitmap_len, path, records, hash))) in
        nb.pages.iter().zip(expected).enumerate()
    {
        assert_eq!(page.index, i);
        assert_eq!(page.address, addr);
        let main = page.layer(LayerSlot::Main).unwrap().bitmap.unwrap();
        assert_eq!((main.address, main.len), (bitmap, bitmap_len));
        let bg = page.layer(LayerSlot::Background).unwrap().bitmap.unwrap();
        assert_eq!(bg.address, 439);
        let tp = page.totalpath.unwrap();
        assert_eq!(tp.address, path);
        assert_eq!(record_count(&nb, path), records);
        assert_eq!(sha256(nb.block(path).unwrap()), hash);
        assert_eq!(page.layer_seq, vec!["MAINLAYER", "BGLAYER"]);
        assert_eq!(page.vectors_current, Some(true));
    }
    assert_eq!(
        nb.block(439).unwrap(),
        open(&fixture("test1.note")).block(330).unwrap()
    );

    assert_eq!(nb.templates.len(), 2);
    let ruled = nb
        .templates
        .iter()
        .find(|t| t.name == "style_8mm_ruled_line")
        .unwrap();
    assert_eq!(
        (ruled.bitmap.unwrap().address, ruled.pages.clone()),
        (439, vec![0, 1, 2])
    );
    let white = nb
        .templates
        .iter()
        .find(|t| t.name == "style_white")
        .unwrap();
    assert_eq!(
        (white.bitmap.unwrap().address, white.bitmap.unwrap().len),
        (905, 324)
    );
    assert!(white.pages.is_empty());

    assert_eq!(nb.covers.len(), 1);
    assert_eq!((nb.covers[0].number, nb.covers[0].block), (0, None));
    assert!(nb.cover().is_none());
}

// ---------------------------------------------------------------- content (11.4)

#[test]
fn content_structure() {
    let Some(path) = local_fixture("content.note") else {
        return;
    };
    let nb = open(&path);
    assert_eq!(nb.bytes().len(), 1681602);
    assert!(nb.warnings.is_empty(), "{:?}", nb.warnings);
    assert_eq!(nb.signature, "SN_FILE_VER_20230015");
    assert_eq!(nb.equipment.as_deref(), Some("A6X"));
    assert_eq!((nb.width, nb.height), (1404, 1872));
    assert!(
        nb.file_id
            .as_deref()
            .is_some_and(|id| id.starts_with('F') && id.len() == 33)
    );
    assert_eq!(nb.dirty, Some(6));
    assert_eq!(nb.saves.len(), 15);
    assert_eq!(nb.blocks.len(), 70);
    assert_eq!(nb.history_error, None);

    let expected = [
        (
            1679987,
            1431290,
            41256,
            1474300,
            76,
            "77db68565a3e55af021717bcd57af973dd61c04873d8e3727232daebe4490689",
        ),
        (
            283760,
            148564,
            29070,
            177902,
            48,
            "becab8806026defc77554bc637ca687b99d3b2b9cc6a09a46fb8c1745fa06121",
        ),
    ];
    assert_eq!(nb.pages.len(), 2);
    for (page, (addr, bitmap, bitmap_len, path, records, hash)) in nb.pages.iter().zip(expected) {
        assert_eq!(page.address, addr);
        let main = page.layer(LayerSlot::Main).unwrap().bitmap.unwrap();
        assert_eq!((main.address, main.len), (bitmap, bitmap_len));
        assert_eq!(page.totalpath.unwrap().address, path);
        assert_eq!(record_count(&nb, path), records);
        assert_eq!(sha256(nb.block(path).unwrap()), hash);
    }
    // a reachable block from the first save (docs/note-format.md 2)
    assert_eq!(
        nb.pages[0].layer(LayerSlot::Background).unwrap().address,
        31288
    );
    // newer firmware keys only on page 1 (docs/note-format.md 4.3)
    assert_eq!(nb.pages[0].page_text_box, Some(0));
    assert_eq!(nb.pages[1].page_text_box, None);

    let addresses: Vec<u64> = nb.links.iter().map(|l| l.address).collect();
    assert_eq!(addresses, vec![452, 843, 1252, 1638, 500079, 1180240]);
    let bitmaps: Vec<(u64, usize)> = nb
        .links
        .iter()
        .map(|l| l.bitmap_block.map(|b| (b.address, b.len)).unwrap())
        .collect();
    assert_eq!(
        bitmaps,
        vec![
            (440, 8),
            (831, 8),
            (1242, 6),
            (1626, 8),
            (496157, 3918),
            (1174576, 5660)
        ]
    );
    for link in &nb.links {
        assert_eq!(link.page, 0);
        // the raw key number stays 1-based
        assert_eq!(link.position.page, 1);
        assert_eq!(link.kind, LinkKind::File);
        assert_eq!(link.direction, Some(LinkDirection::Outgoing));
        assert_eq!(link.protocol.as_deref(), Some("RATTA_RLE"));
        assert_eq!(
            link.bitmap.as_ref().map(Vec::len),
            link.bitmap_block.map(|b| b.len)
        );
        assert_eq!(link.target_page_id, None);
        assert_eq!(link.target_page, Some(0));
        assert!(link.timestamp.is_some());
        assert!(link.target_file_id.is_some());
        assert_eq!(link.rect_original, Some(link.rect));
        // key digits equal the rect (docs/note-format.md 4.1)
        let p = link.position;
        assert_eq!([p.left, p.top, p.width, p.height], link.rect.map(Some));
        let target = link.target.as_deref().unwrap();
        assert!(target.starts_with("/storage/emulated/0/Note/") && target.ends_with(".note"));
        assert!(!link.is_internal(&nb));
    }
    let bike_link = &nb.links[2];
    assert_eq!(
        bike_link.target.as_deref(),
        Some("/storage/emulated/0/Note/Bike.note")
    );
    assert_eq!(
        bike_link.target_file_id.as_deref(),
        Some("F20240708174639550429bHksqaP2wKQ8")
    );
    assert_eq!(bike_link.rect, [788, 426, 275, 111]);
    if let Some(bike) = local_fixture("bike.note") {
        assert_eq!(open(&bike).file_id, bike_link.target_file_id);
    }
    // newer links carry ANTI/BOLD/ITALIC (docs/note-format.md 7.3)
    assert_eq!(nb.links[4].meta.get("BOLD"), Some("1"));
    assert_eq!(nb.links[0].meta.get("BOLD"), None);
}

// ---------------------------------------------------------------- versions

#[test]
fn known_signatures_cover_section_1() {
    assert_eq!(KNOWN_SIGNATURES.len(), 13);
    for sig in [
        "SN_FILE_VER_20230015",
        "SN_FILE_VER_20260016",
        "SN_FILE_VER_20200001",
        "SN_FILE_ASA_20190529",
    ] {
        assert!(KNOWN_SIGNATURES.contains(&sig), "{sig}");
    }
}

fn test2_bytes() -> Vec<u8> {
    std::fs::read(fixture("test2.note")).unwrap()
}

#[test]
fn unknown_version() {
    let mut bytes = test2_bytes();
    bytes[4..24].copy_from_slice(b"SN_FILE_VER_29990001");
    match Notebook::from_bytes(bytes.clone(), VersionPolicy::Strict) {
        Err(NoteError::UnknownVersion(v)) => assert_eq!(v, "SN_FILE_VER_29990001"),
        other => panic!("expected UnknownVersion, got {other:?}"),
    }
    let nb = Notebook::from_bytes(bytes, VersionPolicy::AllowUnknown).unwrap();
    assert!(!nb.version_known);
    assert_eq!(nb.version, Some(29990001));
    assert_eq!(nb.signature, "SN_FILE_VER_29990001");
    assert_eq!(nb.pages.len(), 1);
    assert_eq!(nb.pages[0].address, 239630);
}

#[test]
fn garbage_is_not_a_note() {
    let mut garbage = Vec::new();
    let mut x: u32 = 0x1234_5678;
    for _ in 0..4096 {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        garbage.push((x >> 24) as u8);
    }
    let mut bad_digits = test2_bytes();
    bad_digits[4..24].copy_from_slice(b"SN_FILE_VER_2026001X");
    let mut bad_prefix = test2_bytes();
    bad_prefix[4..16].copy_from_slice(b"XX_FILE_VER_");
    for (name, bytes) in [
        ("empty", Vec::new()),
        ("random", garbage),
        (
            "text",
            b"hello world, this is not a notebook at all".to_vec(),
        ),
        ("bad digits", bad_digits),
        ("bad prefix", bad_prefix),
    ] {
        for policy in [VersionPolicy::Strict, VersionPolicy::AllowUnknown] {
            let result = Notebook::from_bytes(bytes.clone(), policy);
            assert!(
                matches!(result, Err(NoteError::NotANote)),
                "{name}: {result:?}"
            );
        }
    }
}

// ---------------------------------------------------------------- robustness (10)

/// Replaces the last occurrence (current blocks come last in t2).
fn replace(bytes: &mut [u8], from: &[u8], to: &[u8]) {
    assert_eq!(from.len(), to.len());
    let pos = bytes
        .windows(from.len())
        .rposition(|w| w == from)
        .expect("pattern");
    bytes[pos..pos + from.len()].copy_from_slice(to);
}

fn set_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

#[test]
fn mutations_are_rejected_without_panic() {
    let original = test2_bytes();
    let len = original.len();
    type Mutation = Box<dyn Fn(&mut Vec<u8>)>;
    let cases: Vec<(&str, Option<&str>, Mutation)> = vec![
        (
            "footer address past end",
            None,
            Box::new(move |b| set_u32(b, len - 4, len as u32 + 100)),
        ),
        (
            "footer address 0",
            None,
            Box::new(move |b| set_u32(b, len - 4, 0)),
        ),
        (
            "footer address in prefix",
            None,
            Box::new(move |b| set_u32(b, len - 4, 10)),
        ),
        (
            "footer address of a stale footer",
            None,
            Box::new(move |b| set_u32(b, len - 4, 82486)),
        ),
        (
            "tail marker missing",
            None,
            Box::new(move |b| b[len - 8..len - 4].copy_from_slice(b"tall")),
        ),
        (
            "footer length past end",
            None,
            Box::new(|b| set_u32(b, 240904, 0x00FF_FFFF)),
        ),
        (
            "page address past end",
            Some("PAGE1"),
            Box::new(|b| replace(b, b"<PAGE1:239630>", b"<PAGE1:999999>")),
        ),
        (
            "page address is the footer",
            Some("PAGE1"),
            Box::new(|b| replace(b, b"<PAGE1:239630>", b"<PAGE1:240904>")),
        ),
        (
            "page address not a number",
            Some("PAGE1"),
            Box::new(|b| replace(b, b"<PAGE1:239630>", b"<PAGE1:23963x>")),
        ),
        (
            "header address past end",
            Some("FILE_FEATURE"),
            Box::new(|b| replace(b, b"<FILE_FEATURE:24>", b"<FILE_FEATURE:99>")),
        ),
        (
            "page block length past end",
            Some("239630"),
            Box::new(|b| set_u32(b, 239630, 0xFFFF_FF00)),
        ),
        (
            "header block length past end",
            Some("FILE_FEATURE"),
            Box::new(|b| set_u32(b, 24, 0xFFFF_FF00)),
        ),
        (
            "layer block length past end",
            Some("MAINLAYER"),
            Box::new(|b| set_u32(b, 81084, 0x7FFF_0000)),
        ),
        (
            "bitmap block length past end",
            Some("LAYERBITMAP"),
            Box::new(|b| set_u32(b, 2704, 0x7FFF_0000)),
        ),
        (
            "stroke block length past end",
            Some("TOTALPATH"),
            Box::new(|b| set_u32(b, 82569, 0x7FFF_0000)),
        ),
        (
            "page metadata syntax",
            Some("239630"),
            Box::new(|b| replace(b, b"<PAGESTYLE:", b"XPAGESTYLE:")),
        ),
    ];
    for (name, needle, mutate) in cases {
        let mut bytes = original.clone();
        mutate(&mut bytes);
        let result = Notebook::from_bytes(bytes, VersionPolicy::Strict);
        match &result {
            Err(NoteError::Corrupt(message)) => {
                eprintln!("{name}: {message}");
                if let Some(needle) = needle {
                    assert!(
                        message.contains(needle),
                        "{name}: message {message:?} lacks {needle}"
                    );
                }
            }
            Err(NoteError::NotANote) => {}
            other => panic!("{name}: expected Corrupt or NotANote, got {other:?}"),
        }
    }
}

#[test]
fn truncated_files_are_rejected() {
    let original = test2_bytes();
    let len = original.len();
    // no cut lands on the end of an earlier save (2622+..., 82486+...)
    for cut in [
        0,
        4,
        10,
        23,
        24,
        30,
        100,
        2700,
        len / 2,
        len - 100,
        len - 9,
        len - 5,
        len - 1,
    ] {
        let result = Notebook::from_bytes(original[..cut].to_vec(), VersionPolicy::Strict);
        assert!(
            matches!(
                result,
                Err(NoteError::Corrupt(_)) | Err(NoteError::NotANote)
            ),
            "cut at {cut}: {result:?}"
        );
    }
}

#[test]
fn byte_flips_never_panic() {
    let original = test2_bytes();
    for at in (0..original.len()).step_by(211) {
        for value in [0x00, 0xFF, b'<', b'>', b':'] {
            let mut bytes = original.clone();
            bytes[at] = value;
            let _ = Notebook::from_bytes(bytes, VersionPolicy::AllowUnknown);
        }
    }
    // every byte of the current footer and page block
    for at in (239630..original.len()).chain(0..24) {
        let mut bytes = original.clone();
        bytes[at] ^= 0x5A;
        let _ = Notebook::from_bytes(bytes, VersionPolicy::AllowUnknown);
    }
}

// ---------------------------------------------------------------- synthetic blocks

/// Builds a minimal `.note` file block by block (docs/note-format.md 1, 2).
struct Builder {
    data: Vec<u8>,
}

impl Builder {
    fn new(signature: &str) -> Builder {
        let mut data = b"note".to_vec();
        data.extend_from_slice(signature.as_bytes());
        Builder { data }
    }

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

    fn save(&mut self, footer: &[(&str, String)]) -> u64 {
        let address = self.meta(footer);
        self.data.extend_from_slice(b"tail");
        self.data.extend_from_slice(&(address as u32).to_le_bytes());
        address
    }
}

fn s(v: impl ToString) -> String {
    v.to_string()
}

/// A file with a title, a keyword, a web link, a page link and covers, which the
/// samples lack (docs/note-format.md 7.1, 7.2, 7.3, 4.1).
fn synthetic() -> Vec<u8> {
    let mut b = Builder::new("SN_FILE_VER_20230015");
    let header = b.meta(&[
        ("FILE_TYPE", s("NOTE")),
        ("APPLY_EQUIPMENT", s("N5")),
        ("FILE_ID", s("F20250102030405060708abcdefABCDEF")),
        ("FILE_RECOGN_TYPE", s("0")),
    ]);
    let template = b.block(&[0x62, 0xff]);
    let bitmap = b.block(&[0x62, 0x10]);
    let layer = b.meta(&[
        ("LAYERTYPE", s("NOTE")),
        ("LAYERPROTOCOL", s("RATTA_RLE")),
        ("LAYERNAME", s("MAINLAYER")),
        ("LAYERBITMAP", s(bitmap)),
    ]);
    let bg_layer = b.meta(&[
        ("LAYERTYPE", s("NOTE")),
        ("LAYERPROTOCOL", s("RATTA_RLE")),
        ("LAYERNAME", s("MAINLAYER")), // firmware quirk, docs/note-format.md 4.5
        ("LAYERBITMAP", s(template)),
    ]);
    let layerinfo = r##"[{"layerId"#0,"name"#"Ink","isBackgroundLayer"#false,"isVisible"#false},{"layerId"#-1,"name"#"Paper","isBackgroundLayer"#true,"isVisible"#true}]"##;
    let page1 = b.meta(&[
        ("PAGESTYLE", s("style_white")),
        ("PAGESTYLEMD5", s("0")),
        ("LAYERINFO", s(layerinfo)),
        ("LAYERSEQ", s("MAINLAYER,BGLAYER")),
        ("MAINLAYER", s(layer)),
        ("LAYER1", s("0")),
        ("BGLAYER", s(bg_layer)),
        ("TOTALPATH", s("0")),
        ("PAGEID", s("P20250102030405070000abcdefABCDEF")),
        ("ORIENTATION", s("1090")),
        ("FIVESTAR", s("1,2,3,4,1")),
        ("FUTUREKEY", s("kept")),
    ]);
    let page2 = b.meta(&[
        ("PAGESTYLE", s("style_white")),
        ("LAYERINFO", s("none")),
        ("PAGEID", s("none")),
    ]);
    let title_bitmap = b.block(&[0x61, 0x05]);
    let title_a = b.meta(&[
        ("TITLESEQNO", s("0")),
        ("TITLELEVEL", s("2")),
        ("TITLERECT", s("100,200,300,40")),
        ("TITLERECTORI", s("100,200,300,40")),
        ("TITLEBITMAP", s(title_bitmap)),
        ("TITLEPROTOCOL", s("RATTA_RLE")),
        ("TITLESTYLE", s("1157254")),
    ]);
    let title_b = b.meta(&[
        ("TITLELEVEL", s("1")),
        ("TITLERECTORI", s("100,200,50,40")),
        ("TITLEBITMAP", s("0")),
    ]);
    let site = b.block("Grüße".as_bytes());
    let keyword = b.meta(&[
        ("KEYWORDPAGE", s("0")),
        ("KEYWORDSEQNO", s("3")),
        ("KEYWORDRECT", s("10,500,80,30")),
        ("KEYWORDRECTORI", s("10,500,80,30")),
        ("KEYWORDSITE", s(site)),
        ("KEYWORDLEN", s("6")),
        ("KEYWORD", s("inline")),
    ]);
    let url = "aHR0cHM6Ly9leGFtcGxlLmNvbS8="; // https://example.com/
    let web = b.meta(&[
        ("LINKTYPE", s("4")),
        ("LINKINOUT", s("1")),
        ("LINKBITMAP", s("0")),
        ("LINKTIMESTAMP", s("20250102030405999999")),
        ("LINKRECT", s("5,6,7,8")),
        ("LINKFILE", s(url)),
        ("LINKFILEID", s("none")),
        ("PAGEID", s("none")),
        ("OBJPAGE", s("0")),
    ]);
    let internal = b.meta(&[
        ("LINKTYPE", s("0")),
        ("LINKINOUT", s("0")),
        ("LINKRECT", s("1,2,3,4")),
        ("LINKFILE", s("none")),
        ("LINKFILEID", s("F20250102030405060708abcdefABCDEF")),
        ("PAGEID", s("P20250102030405070000abcdefABCDEF")),
        ("OBJPAGE", s("1")),
    ]);
    let other = b.meta(&[("LINKTYPE", s("2")), ("LINKRECT", s("1,1,1,1"))]);
    let cover1 = b.block(b"cover one");
    let cover2 = b.block(b"cover two");
    b.save(&[
        ("PAGE2", s(page2)),
        ("PAGE1", s(page1)),
        ("TITLE_000202000100", s(title_a)),
        ("TITLE_000202000100", s(title_b)),
        ("KEYWORD_00010500", s(keyword)),
        ("LINKO_00020006000500080007", s(web)),
        ("LINKO_00010002000100040003", s(internal)),
        ("LINKO_00010001000100010001", s(other)),
        ("COVER_1", s(cover1)),
        ("COVER_2", s(cover2)),
        ("DIRTY", s("3")),
        ("FILE_FEATURE", s(header)),
        ("STYLE_style_white", s(template)),
    ]);
    b.data
}

#[test]
fn synthetic_metadata_blocks() {
    let nb = Notebook::from_bytes(synthetic(), VersionPolicy::Strict).unwrap();
    assert_eq!(nb.warnings, Vec::<String>::new());
    assert_eq!(nb.equipment.as_deref(), Some("N5"));
    assert_eq!((nb.width, nb.height), (1920, 2560));
    assert_eq!(nb.dirty, Some(3));

    // pages sorted by the number in the key (docs/note-format.md 3)
    assert_eq!(
        nb.pages.iter().map(|p| p.index).collect::<Vec<_>>(),
        vec![0, 1]
    );
    let page = &nb.pages[0];
    assert!(page.landscape);
    assert_eq!((page.width, page.height), (2560, 1920));
    assert_eq!(page.five_star.as_deref(), Some("1,2,3,4,1"));
    assert_eq!(page.meta.get("FUTUREKEY"), Some("kept"));
    assert_eq!(page.totalpath, None);
    assert_eq!(page.vectors_current, Some(false));
    let main = page.layer(LayerSlot::Main).unwrap();
    assert!(!main.visible);
    assert_eq!(main.display_name.as_deref(), Some("Ink"));
    let bg = page.layer(LayerSlot::Background).unwrap();
    assert_eq!(bg.name, "BGLAYER");
    assert_eq!(bg.layer_name.as_deref(), Some("MAINLAYER"));
    assert!(bg.visible);
    assert_eq!(bg.display_name.as_deref(), Some("Paper"));
    assert_eq!(nb.pages[1].layer_info, None);
    assert_eq!(nb.pages[1].id, None);
    assert_eq!(nb.templates[0].pages, vec![0, 1]);

    // two titles under the same key (docs/note-format.md 3)
    assert_eq!(nb.titles.len(), 2);
    let title = &nb.titles[0];
    assert_eq!(title.page, 1);
    assert_eq!(title.position.page, 2);
    assert_eq!(
        (title.position.top, title.position.left),
        (Some(200), Some(100))
    );
    assert_eq!(title.rect, [100, 200, 300, 40]);
    assert_eq!(title.level, Some(2));
    assert_eq!(title.seq_no, Some(0));
    assert_eq!(title.protocol.as_deref(), Some("RATTA_RLE"));
    let style = title.style.unwrap();
    assert_eq!(
        (style.code, style.background_gray, style.text_gray),
        (1157254, 157, 254)
    );
    assert_eq!(title.bitmap.as_deref(), Some(&[0x61, 0x05][..]));
    let second = &nb.titles[1];
    assert_eq!(second.rect, [100, 200, 50, 40]);
    assert_eq!(second.level, Some(1));
    assert_eq!(second.bitmap, None);

    assert_eq!(nb.keywords.len(), 1);
    let keyword = &nb.keywords[0];
    assert_eq!(keyword.page, 0);
    assert_eq!(keyword.position.page, 1);
    assert_eq!(keyword.keyword_page, Some(0));
    assert_eq!(keyword.position.top, Some(500));
    assert_eq!(keyword.rect, [10, 500, 80, 30]);
    assert_eq!(keyword.text.as_deref(), Some("Grüße"));
    assert_eq!(keyword.inline_text.as_deref(), Some("inline"));
    assert_eq!(keyword.seq_no, Some(3));
    assert_eq!(keyword.len, Some(6));

    // links sorted by page, top, left
    assert_eq!(nb.links.len(), 3);
    let kinds: Vec<LinkKind> = nb.links.iter().map(|l| l.kind).collect();
    assert_eq!(
        kinds,
        vec![LinkKind::Other(2), LinkKind::Page, LinkKind::Web]
    );
    let web = &nb.links[2];
    assert_eq!(web.page, 1);
    assert_eq!(web.direction, Some(LinkDirection::Incoming));
    assert_eq!(web.target.as_deref(), Some("https://example.com/"));
    assert_eq!(web.target_file_id, None);
    assert_eq!(web.rect, [5, 6, 7, 8]);
    assert_eq!(
        web.timestamp.map(|t| t.to_string()).as_deref(),
        Some("2025-01-02 03:04:05.999999")
    );
    assert_eq!(web.bitmap, None);
    let internal = &nb.links[1];
    assert_eq!(internal.target, None);
    assert!(internal.is_internal(&nb));
    assert_eq!(
        internal.target_page_id.as_deref(),
        Some("P20250102030405070000abcdefABCDEF")
    );
    assert_eq!(internal.target_page, Some(1));
    assert_eq!(nb.links[0].direction, None);

    assert_eq!(nb.covers.len(), 2);
    assert_eq!(
        nb.cover().and_then(|c| c.data.as_deref()),
        Some(&b"cover two"[..])
    );
    assert_eq!(nb.saves.len(), 1);
    assert!(nb.blocks.iter().all(|b| b.reachable));
}

#[test]
fn synthetic_link_without_rect_is_corrupt() {
    let mut b = Builder::new("SN_FILE_VER_20230015");
    let header = b.meta(&[("FILE_TYPE", s("NOTE"))]);
    let page = b.meta(&[("PAGESTYLE", s("style_white"))]);
    let link = b.meta(&[("LINKTYPE", s("1"))]);
    b.save(&[
        ("FILE_FEATURE", s(header)),
        ("PAGE1", s(page)),
        ("LINKO_00010001000100010001", s(link)),
    ]);
    match Notebook::from_bytes(b.data, VersionPolicy::Strict) {
        Err(NoteError::Corrupt(m)) => assert!(m.contains("LINKRECT"), "{m}"),
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

#[test]
fn synthetic_bad_layerinfo_is_kept_raw_with_a_warning() {
    let mut b = Builder::new("SN_FILE_VER_20230015");
    let header = b.meta(&[("FILE_TYPE", s("NOTE"))]);
    let page = b.meta(&[("LAYERINFO", s("[{broken")), ("RECOGNSTATUS", s("x"))]);
    b.save(&[("FILE_FEATURE", s(header)), ("PAGE1", s(page))]);
    let nb = Notebook::from_bytes(b.data, VersionPolicy::Strict).unwrap();
    assert_eq!(nb.pages[0].layer_info, None);
    assert_eq!(nb.pages[0].recogn_status, None);
    assert_eq!(nb.pages[0].meta.get("LAYERINFO"), Some("[{broken"));
    assert_eq!(nb.warnings.len(), 2, "{:?}", nb.warnings);
    assert!(!nb.device_known);
    assert_eq!((nb.width, nb.height), (1404, 1872));
}

/// Footer with one header, the given pages (key, has a block) and one link key.
fn paged(pages: &[&str], link_key: Option<&str>) -> Vec<u8> {
    let mut b = Builder::new("SN_FILE_VER_20230015");
    let header = b.meta(&[("FILE_TYPE", s("NOTE"))]);
    let mut footer = vec![("FILE_FEATURE", s(header))];
    for key in pages {
        let page = b.meta(&[("PAGEID", format!("id-{key}"))]);
        footer.push((key, s(page)));
    }
    if let Some(key) = link_key {
        let link = b.meta(&[("LINKTYPE", s("1")), ("LINKRECT", s("1,2,3,4"))]);
        footer.push((key, s(link)));
    }
    b.save(&footer);
    b.data
}

#[test]
fn gapped_page_keys_give_dense_positions() {
    let bytes = paged(&["PAGE3", "PAGE1"], Some("LINKO_00030001000100010001"));
    let nb = Notebook::from_bytes(bytes, VersionPolicy::Strict).unwrap();
    let ids: Vec<(usize, Option<&str>)> = nb
        .pages
        .iter()
        .map(|p| (p.index, p.id.as_deref()))
        .collect();
    assert_eq!(ids, vec![(0, Some("id-PAGE1")), (1, Some("id-PAGE3"))]);
    assert_eq!(nb.page(1).and_then(|p| p.id.as_deref()), Some("id-PAGE3"));
    assert_eq!(nb.links[0].page, 1);
    assert_eq!(nb.links[0].position.page, 3);
    assert_eq!(
        nb.saves[0]
            .page_addresses
            .iter()
            .map(|p| p.0)
            .collect::<Vec<_>>(),
        vec![1, 3]
    );
}

#[test]
fn page_number_zero_is_corrupt() {
    for (name, bytes, needle) in [
        ("PAGE0", paged(&["PAGE0"], None), "PAGE0"),
        (
            "item page 0000",
            paged(&["PAGE1"], Some("LINKO_00000001000100010001")),
            "LINKO_00000001000100010001",
        ),
        (
            "item on a page without key",
            paged(&["PAGE1"], Some("LINKO_00020001000100010001")),
            "LINKO_00020001000100010001",
        ),
    ] {
        match Notebook::from_bytes(bytes, VersionPolicy::Strict) {
            Err(NoteError::Corrupt(m)) => assert!(m.contains(needle), "{name}: {m}"),
            other => panic!("{name}: expected Corrupt, got {other:?}"),
        }
    }
}
