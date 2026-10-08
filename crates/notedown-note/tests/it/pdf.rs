//! PDF output (docs/superpowers/specs/2026-10-08-notedown-render-and-push-order-design.md 4.1).

use std::path::PathBuf;

use notedown_note::{Notebook, RenderOptions, RenderStyle, VersionPolicy};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn open(name: &str) -> Notebook {
    Notebook::open(
        &root().join(format!("tests/fixtures/notes/{name}.note")),
        VersionPolicy::Strict,
    )
    .unwrap()
}

fn with_template() -> RenderOptions {
    RenderOptions {
        include_background: true,
        style: RenderStyle::Reference,
    }
}

/// The decompressed image streams and the MediaBoxes of `pdf`, by a small reader for the
/// shape `render_pdf` writes (one image XObject per page, FlateDecode).
fn images_and_boxes(pdf: &[u8]) -> (Vec<Vec<u8>>, Vec<[f32; 4]>) {
    let text = String::from_utf8_lossy(pdf);
    let mut boxes = Vec::new();
    for part in text.split("/MediaBox [").skip(1) {
        let nums: Vec<f32> = part
            .split(']')
            .next()
            .unwrap()
            .split_whitespace()
            .map(|n| n.parse().unwrap())
            .collect();
        boxes.push([nums[0], nums[1], nums[2], nums[3]]);
    }
    let mut images = Vec::new();
    let mut rest = pdf;
    while let Some(at) = find(rest, b"/Subtype /Image") {
        let after = &rest[at..];
        let start = find(after, b"stream\n").unwrap() + b"stream\n".len();
        let end = find(&after[start..], b"\nendstream").unwrap();
        let data = &after[start..start + end];
        images.push(miniz_oxide::inflate::decompress_to_vec_zlib(data).unwrap());
        rest = &after[start + end..];
    }
    (images, boxes)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[test]
fn a_pdf_has_one_page_per_note_page_with_the_rendered_gray_bytes() {
    for name in ["test1", "test2"] {
        let nb = open(name);
        let pdf = nb.render_pdf(&with_template()).unwrap();
        assert!(pdf.starts_with(b"%PDF-"));
        let (images, boxes) = images_and_boxes(&pdf);
        assert_eq!(images.len(), nb.pages.len());
        assert_eq!(boxes.len(), nb.pages.len());
        for (i, gray) in images.iter().enumerate() {
            let img = nb.render_page(i, &with_template()).unwrap();
            let expected: Vec<u8> = img
                .pixels()
                .map(|p| if p.0[3] == 0 { 255 } else { p.0[0] })
                .collect();
            assert_eq!(gray, &expected, "{name} page {i}");
            // 1404 x 1872 px at 300 ppi
            let want = [0.0, 0.0, 336.96, 449.28];
            for (g, w) in boxes[i].iter().zip(want) {
                assert!((g - w).abs() < 0.01, "{name} page {i}: {:?}", boxes[i]);
            }
        }
    }
}

#[test]
fn transparent_pixels_are_white_without_template() {
    let nb = open("test1");
    let opts = RenderOptions {
        include_background: false,
        style: RenderStyle::Reference,
    };
    let (images, _) = images_and_boxes(&nb.render_pdf(&opts).unwrap());
    let img = nb.render_page(0, &opts).unwrap();
    for (got, px) in images[0].iter().zip(img.pixels()) {
        let want = if px.0[3] == 0 { 255 } else { px.0[0] };
        assert_eq!(*got, want);
    }
}

#[test]
fn the_same_note_gives_the_same_bytes() {
    let nb = open("test2");
    assert_eq!(
        nb.render_pdf(&with_template()).unwrap(),
        nb.render_pdf(&with_template()).unwrap()
    );
}

#[test]
fn ppi_follows_the_device() {
    let mut nb = open("test1");
    assert_eq!(nb.ppi(), 300);
    nb.equipment = Some("A5X".into());
    assert_eq!(nb.ppi(), 226);
    nb.equipment = Some("XYZ".into());
    assert_eq!(nb.ppi(), 300);
    nb.equipment = None;
    assert_eq!(nb.ppi(), 300);
}
