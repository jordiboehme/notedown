//! Bitmap decoding and page rendering against docs/note-format.md 5, 6 and 11.
//!
//! The local fixtures (bike, content) are private: only counts and hashes are
//! asserted for them, and every test skips with a note when they are missing.

use std::path::PathBuf;

use notedown_note::{
    NoteError, Notebook, RenderOptions, RenderStyle, VersionPolicy, decode_rle, encode_png, gray_x2,
};
use sha2::{Digest, Sha256};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn open(path: PathBuf) -> Notebook {
    match Notebook::open(&path, VersionPolicy::Strict) {
        Ok(nb) => nb,
        Err(e) => panic!("cannot open {}: {e}", path.display()),
    }
}

fn load_png(path: PathBuf) -> image::RgbaImage {
    match image::open(&path) {
        Ok(img) => img.to_rgba8(),
        Err(e) => panic!("cannot read {}: {e}", path.display()),
    }
}

fn diff_pixels(a: &image::RgbaImage, b: &image::RgbaImage) -> usize {
    assert_eq!(a.dimensions(), b.dimensions());
    a.pixels().zip(b.pixels()).filter(|(x, y)| x != y).count()
}

fn sha256(bytes: &[u8]) -> String {
    const_hex::encode(Sha256::digest(bytes))
}

/// (name, notebook, golden dir) for every sample that is present.
fn samples() -> Vec<(&'static str, Notebook, PathBuf)> {
    let mut out = Vec::new();
    for name in ["test1", "test2"] {
        out.push((
            name,
            open(root().join(format!("tests/fixtures/notes/{name}.note"))),
            root().join("tests/fixtures/golden"),
        ));
    }
    for name in ["bike", "content"] {
        let path = root().join(format!("tests/fixtures/local/{name}.note"));
        if path.exists() {
            out.push((name, open(path), root().join("tests/fixtures/local/golden")));
        } else {
            eprintln!("skipping: local fixture {} is missing", path.display());
        }
    }
    out
}

#[test]
fn reference_render_matches_goldens() {
    for (name, nb, golden) in samples() {
        for index in 0..nb.pages.len() {
            for (bg, suffix) in [(false, "nobg"), (true, "bg")] {
                let opts = RenderOptions {
                    include_background: bg,
                    style: RenderStyle::Reference,
                };
                let img = nb.render_page(index, &opts).expect("render");
                let expected = load_png(golden.join(format!("{name}-p{}-{suffix}.png", index + 1)));
                assert_eq!(
                    diff_pixels(&img, &expected),
                    0,
                    "{name} page {index} {suffix}"
                );
            }
        }
    }
}

#[test]
fn default_options_are_reference_without_background() {
    let d = RenderOptions::default();
    assert!(!d.include_background);
    assert_eq!(d.style, RenderStyle::Reference);
}

fn gray_hash(codes: &[u8]) -> String {
    let gray: Vec<u8> = codes.iter().map(|&c| gray_x2(c).unwrap_or(255)).collect();
    sha256(&gray)
}

// docs/note-format.md 11.1: the shared ruled template (t1, t2, bike all use the same bytes).
const BG_CODES: &str = "04238c0017e9a91e915a3d8ce2f5b78779e34d45d7e6fbe8a740be76b27761d4";
const BG_GRAY: &str = "2b7209c3f1c749fa26a43d54acdf52b11934d23554f249c6e9bbfe3125ba3658";

#[test]
fn layer_vectors_test1_and_test2() {
    let t1 = open(root().join("tests/fixtures/notes/test1.note"));
    let main = t1.decode_layer(0, "MAINLAYER").expect("t1 main");
    assert_eq!(main.codes.len(), 2_628_288);
    assert_eq!((main.width, main.height), (1404, 1872));
    assert_eq!(main.opaque, 34435);
    assert_eq!(
        sha256(&main.codes),
        "5d901d57bcbdd9660225cc667a397da49b071942451c401f6139d8d9e0b12510"
    );
    assert_eq!(
        gray_hash(&main.codes),
        "141fc8ccf4681ee1e921a2eb106bd1ef8411d95d7fdcf25f2db302b13e41e332"
    );
    let bg = t1.decode_layer(0, "BGLAYER").expect("t1 bg");
    assert_eq!(bg.opaque, 21168);
    assert_eq!(sha256(&bg.codes), BG_CODES);
    assert_eq!(gray_hash(&bg.codes), BG_GRAY);

    let t2 = open(root().join("tests/fixtures/notes/test2.note"));
    let main = t2.decode_layer(0, "MAINLAYER").expect("t2 main");
    assert_eq!(main.codes.len(), 2_628_288);
    assert_eq!(main.opaque, 65293);
    assert_eq!(
        sha256(&main.codes),
        "5ccef18d500034f6d7b1b2a65bc2c9af865c17926eed4be5ef72f68200f59b88"
    );
    assert_eq!(
        gray_hash(&main.codes),
        "2eab8aa3128e701e2639129d755cb1c73652cce4fb5df84d46d773a52cb4714c"
    );
    let bg = t2.decode_layer(0, "BGLAYER").expect("t2 bg");
    assert_eq!(sha256(&bg.codes), BG_CODES);
}

#[test]
fn layer_vectors_local_samples() {
    // (file, page, opaque, sha256 of the gray buffer), docs/note-format.md 11.3 and 11.4
    let table = [
        (
            "bike",
            0,
            86472,
            "be9b49f0bd88405f67d067a8293e1f3036f8199bf3bf02645bddba1a7bd4414a",
        ),
        (
            "bike",
            1,
            66035,
            "c80dc3bd7346d0f41f4980eb17e93c0abebd4bd15854ef9937867170261f9af8",
        ),
        (
            "bike",
            2,
            18351,
            "7363342760ba0121ca5c50ea2ad30717cfd5e13a09b5577acb9adf17d50cb616",
        ),
        (
            "content",
            0,
            40903,
            "2f951b16a780880d67b118b4deea9d91dce9a3b6e9246e270fff77856a08a2d1",
        ),
        (
            "content",
            1,
            21879,
            "f1dfb516dced09dba4d53f0847bd600c693846f97d365da0ab838e02cf4579bf",
        ),
    ];
    for name in ["bike", "content"] {
        let path = root().join(format!("tests/fixtures/local/{name}.note"));
        if !path.exists() {
            eprintln!("skipping: local fixture {} is missing", path.display());
            continue;
        }
        let nb = open(path);
        for (file, page, opaque, hash) in table {
            if file != name {
                continue;
            }
            let main = nb.decode_layer(page, "MAINLAYER").expect("main layer");
            assert_eq!(main.codes.len(), 2_628_288, "{name} page {page}");
            assert_eq!(main.opaque, opaque, "{name} page {page}");
            assert_eq!(gray_hash(&main.codes), hash, "{name} page {page}");
        }
        for page in 0..nb.pages.len() {
            let bg = nb.decode_layer(page, "BGLAYER").expect("background");
            assert_eq!(bg.codes.len(), 2_628_288);
            if name == "bike" {
                assert_eq!(sha256(&bg.codes), BG_CODES, "bike page {page} template");
            }
        }
    }
}

#[test]
fn unknown_layer_and_page_are_errors() {
    let t1 = open(root().join("tests/fixtures/notes/test1.note"));
    assert!(matches!(
        t1.decode_layer(0, "LAYER2"),
        Err(NoteError::Decode(_))
    ));
    assert!(matches!(
        t1.decode_layer(5, "MAINLAYER"),
        Err(NoteError::Decode(_))
    ));
    assert!(t1.render_page(5, &RenderOptions::default()).is_err());
    assert!(t1.page_is_blank(5).is_err());
}

#[test]
fn sample_pages_are_not_blank() {
    for (name, nb, _) in samples() {
        for index in 0..nb.pages.len() {
            assert!(
                !nb.page_is_blank(index).expect("blank check"),
                "{name} page {index}"
            );
        }
    }
}

/// RLE bytes for `pixels` transparent pixels, built from the rules of docs/note-format.md 5.2.
fn transparent_stream(pixels: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = pixels;
    while rest >= 0x4000 {
        out.extend([0x62, 0xFF]);
        rest -= 0x4000;
    }
    if rest >= 128 {
        let high = rest / 128 - 1;
        out.extend([0x62, 0x80 | u8::try_from(high).expect("high part")]);
        rest -= (high + 1) * 128;
        if rest > 0 {
            out.extend([0x62, u8::try_from(rest - 1).expect("low part")]);
        }
    } else if rest > 0 {
        out.extend([0x62, u8::try_from(rest - 1).expect("low part")]);
    }
    out
}

#[test]
fn all_transparent_stream_is_blank() {
    let (w, h) = (1404u32, 1872u32);
    let n = (w * h) as usize;
    let stream = transparent_stream(n);
    // the synthetic stream is the style_white template of docs/note-format.md 5.2
    assert_eq!(stream.len(), 324);
    let codes = decode_rle(&stream, n).expect("decode");
    assert!(codes.iter().all(|&c| c == 0x62));
    let layer = notedown_note::DecodedLayer {
        width: w,
        height: h,
        opaque: 0,
        codes,
    };
    assert!(layer.is_blank());
    let mut inked = layer.clone();
    inked.codes[7] = 0x61;
    assert!(!inked.is_blank());
}

#[test]
fn rle_rules() {
    // plain runs: len + 1 pixels
    assert_eq!(decode_rle(&[0x61, 0x02], 3).expect("run"), vec![0x61; 3]);
    // high part with the same code: (h + 1) * 128 + l + 1
    let mut want = vec![0x61; 134];
    assert_eq!(
        decode_rle(&[0x61, 0x80, 0x61, 0x05], 134).expect("pair"),
        want
    );
    // high part with a different code: (h + 1) * 128, then the next pair is normal
    want = vec![0x62; 128];
    want.push(0xDF);
    assert_eq!(
        decode_rle(&[0x62, 0x80, 0xDF, 0x00], 129).expect("high alone"),
        want
    );
    // 0xFF is 16384 pixels
    assert_eq!(
        decode_rle(&[0x62, 0xFF], 16384).expect("blank").len(),
        16384
    );
    // errors: short, long, odd, malformed high part
    for (data, expected) in [
        (&[0x61u8, 0x02][..], 4usize),
        (&[0x61, 0x02], 2),
        (&[0x61, 0x02, 0x61], 3),
        (&[0x61, 0x80, 0x61, 0x85], 140),
        (&[0x62, 0xFF], 100),
        (&[], 1),
    ] {
        assert!(
            matches!(decode_rle(data, expected), Err(NoteError::Decode(_))),
            "{data:?} -> {expected}"
        );
    }
    // the message names expected and actual length
    let Err(NoteError::Decode(m)) = decode_rle(&[0x61, 0x02], 4) else {
        panic!("expected a decode error");
    };
    assert!(m.contains('4') && m.contains('3'), "{m}");
}

#[test]
fn decoder_survives_truncated_and_random_input() {
    let t2 = open(root().join("tests/fixtures/notes/test2.note"));
    let page = t2.page(0).expect("page");
    let layer = page
        .layers
        .iter()
        .find(|l| l.name == "MAINLAYER")
        .expect("main");
    let data = t2
        .block(layer.bitmap.expect("bitmap").address)
        .expect("payload");
    let n = 1404 * 1872;
    assert!(decode_rle(data, n).is_ok());
    for cut in (0..data.len())
        .step_by(997)
        .chain([1, 2, 3, data.len() - 1, data.len() - 2])
    {
        assert!(decode_rle(&data[..cut], n).is_err(), "cut at {cut}");
    }
    // appended input after a complete image is an error
    let mut longer = data.to_vec();
    longer.extend([0x62, 0x00]);
    assert!(decode_rle(&longer, n).is_err());
    // xorshift random streams of many lengths and a small target
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for round in 0..2000 {
        let len = (next() % 200) as usize;
        let bytes: Vec<u8> = (0..len).map(|_| (next() >> 24) as u8).collect();
        let target = if round % 2 == 0 {
            n
        } else {
            1 + (next() % 5000) as usize
        };
        let _ = decode_rle(&bytes, target);
        if round % 2 == 0 {
            assert!(decode_rle(&bytes, n).is_err());
        }
    }
}

#[test]
fn unsupported_protocol_names_it() {
    // a PNG block and an unknown protocol are reported by name (docs/note-format.md 5.1)
    let t1 = open(root().join("tests/fixtures/notes/test1.note"));
    let page = t1.page(0).expect("page");
    let mut layer = page
        .layers
        .iter()
        .find(|l| l.name == "MAINLAYER")
        .expect("main")
        .clone();
    layer.protocol = "SN_ASA_COMPRESS".into();
    let Err(NoteError::Decode(m)) = t1.decode_page_layer(page, &layer) else {
        panic!("expected a decode error");
    };
    assert!(m.contains("SN_ASA_COMPRESS"), "{m}");
}

#[test]
fn encode_png_round_trips() {
    let t1 = open(root().join("tests/fixtures/notes/test1.note"));
    let opts = RenderOptions {
        include_background: true,
        style: RenderStyle::Reference,
    };
    let img = t1.render_page(0, &opts).expect("render");
    let png = encode_png(&img).expect("encode");
    assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
    let back = image::load_from_memory(&png).expect("decode").to_rgba8();
    assert_eq!(diff_pixels(&img, &back), 0);
}

#[test]
fn link_bitmaps_decode_to_rect_size() {
    let path = root().join("tests/fixtures/local/content.note");
    if !path.exists() {
        eprintln!("skipping: local fixture {} is missing", path.display());
        return;
    }
    let nb = open(path);
    assert_eq!(nb.links.len(), 6);
    for link in &nb.links {
        let d = nb.decode_link_bitmap(link).expect("link bitmap");
        assert_eq!((d.width, d.height), (link.rect[2], link.rect[3]));
        assert_eq!(d.codes.len(), (link.rect[2] * link.rect[3]) as usize);
    }
}

#[test]
fn device_style_against_the_device_export() {
    // docs/note-format.md 6.2: exact for both samples with a device export
    for name in ["test1", "test2"] {
        let nb = open(root().join(format!("tests/fixtures/notes/{name}.note")));
        let opts = RenderOptions {
            include_background: true,
            style: RenderStyle::Device,
        };
        let img = nb.render_page(0, &opts).expect("render");
        let device = load_png(root().join(format!("tests/fixtures/device/{name}-page1.png")));
        assert_eq!(diff_pixels(&img, &device), 0, "{name}");
    }
}

#[test]
fn huge_sizes_are_errors_not_allocations() {
    // a tiny stream with a huge expected length: capacity is bounded by the stream
    assert!(matches!(
        decode_rle(&[0x62, 0xFF], usize::MAX / 2),
        Err(NoteError::Decode(_))
    ));
    assert!(matches!(
        decode_rle(&[0x62, 0xFF], 1 << 40),
        Err(NoteError::Decode(_))
    ));
    // rect sizes come from the file: 100000 x 100000 is rejected before any allocation
    let path = root().join("tests/fixtures/local/content.note");
    if !path.exists() {
        eprintln!("skipping: local fixture {} is missing", path.display());
        return;
    }
    let nb = open(path);
    let base = nb.links.first().expect("a link").clone();
    for rect in [
        [0, 0, 100_000, 100_000],
        [0, 0, u32::MAX, u32::MAX],
        [0, 0, 20_000, 1],
    ] {
        let mut link = base.clone();
        link.rect = rect;
        let Err(NoteError::Decode(m)) = nb.decode_link_bitmap(&link) else {
            panic!("expected a decode error for {rect:?}");
        };
        assert!(m.contains("implausible") && m.contains("link"), "{m}");
    }
}

#[test]
fn huge_page_size_is_an_error() {
    // page sizes use the same check as link and title rects (decode_payload)
    let t1 = open(root().join("tests/fixtures/notes/test1.note"));
    let mut page = t1.page(0).expect("page").clone();
    page.width = 100_000;
    page.height = 100_000;
    let layer = page.layers.first().expect("layer").clone();
    let Err(NoteError::Decode(m)) = t1.decode_page_layer(&page, &layer) else {
        panic!("expected a decode error");
    };
    assert!(m.contains("implausible"), "{m}");
}

#[test]
fn gray_x2_is_the_device_involution() {
    let pairs = [
        (0x61, 0x00),
        (0x63, 0x30),
        (0x64, 0x50),
        (0x65, 0xFE),
        (0x66, 0x01),
        (0x67, 0x31),
        (0x68, 0x51),
    ];
    for (a, b) in pairs {
        assert_eq!(gray_x2(a), Some(b), "code {a:#04x}");
        assert_eq!(gray_x2(b), Some(a), "code {b:#04x}");
    }
    assert_eq!(gray_x2(0x62), None);
    assert_eq!(gray_x2(0xFF), Some(0x62));
    for code in [0x9D, 0x9E, 0xC9, 0xCA, 0x0F, 0x1F, 0xEF, 0x02, 0x60, 0x69] {
        assert_eq!(gray_x2(code), Some(code), "code {code:#04x}");
    }
}
