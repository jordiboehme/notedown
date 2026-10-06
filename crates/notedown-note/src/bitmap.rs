//! Bitmap decoding (docs/note-format.md 5, 7, 6.3).
//!
//! Only `RATTA_RLE` is decoded (5.1, 5.2). `SN_ASA_COMPRESS` and PNG template
//! blocks are recognised and rejected with an error that names the protocol.

use crate::error::NoteError;
use crate::model::{Layer, LayerSlot, Link, Page, Title};
use crate::notebook::Notebook;

/// The code of a transparent pixel (docs/note-format.md 5.3).
pub const CODE_TRANSPARENT: u8 = 0x62;

/// The protocol name of the only decoded encoding (docs/note-format.md 5.1).
pub const PROTOCOL_RLE: &str = "RATTA_RLE";

/// Run length of a `len == 0xFF` pair (docs/note-format.md 5.2).
const BLANK_RUN: usize = 0x4000;
/// Run length of a `len == 0xFF` pair in the old `style_white` quirk (5.2).
const BLANK_RUN_QUIRK: usize = 1024;
/// Block length that selects the quirk (docs/note-format.md 5.2).
const QUIRK_BLOCK_LEN: usize = 0x140E;

/// A decoded bitmap: the raw code per pixel, row-major, top row first
/// (docs/note-format.md Appendix A, "Bitmap").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedLayer {
    pub width: u32,
    pub height: u32,
    /// `width * height` raw code values (docs/note-format.md 11, "codes").
    pub codes: Vec<u8>,
    /// Pixels whose code is not `0x62`.
    pub opaque: usize,
}

impl DecodedLayer {
    fn new(width: u32, height: u32, codes: Vec<u8>) -> DecodedLayer {
        let opaque = codes.iter().filter(|&&c| c != CODE_TRANSPARENT).count();
        DecodedLayer {
            width,
            height,
            codes,
            opaque,
        }
    }

    /// True when every pixel is transparent or white (`0x65`).
    pub fn is_blank(&self) -> bool {
        self.codes
            .iter()
            .all(|&c| c == CODE_TRANSPARENT || c == 0x65)
    }
}

/// Decodes a `RATTA_RLE` stream to exactly `expected` codes (docs/note-format.md 5.2, 10.5).
///
/// Never reads past `data` and never produces more or fewer than `expected` pixels:
/// an odd stream, a malformed high part, a run past the end or a short output is
/// [`NoteError::Decode`] with the expected and the actual length.
pub fn decode_rle(data: &[u8], expected: usize) -> Result<Vec<u8>, NoteError> {
    decode_rle_with(data, expected, BLANK_RUN)
}

fn decode_rle_with(data: &[u8], expected: usize, blank_run: usize) -> Result<Vec<u8>, NoteError> {
    if !data.len().is_multiple_of(2) {
        return Err(NoteError::Decode(format!(
            "{PROTOCOL_RLE}: odd stream length {}",
            data.len()
        )));
    }
    let mut pairs = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&[code, len]| (code, len))
        .peekable();
    let mut out: Vec<u8> = Vec::with_capacity(expected.min(data.len() / 2 * BLANK_RUN));
    while let Some((code, len)) = pairs.next() {
        let run = if len == 0xFF {
            blank_run
        } else if len & 0x80 == 0 {
            usize::from(len) + 1
        } else {
            let high = usize::from(len & 0x7F);
            match pairs.peek() {
                Some(&(next_code, next_len)) if next_code == code => {
                    if next_len >= 0x80 {
                        return Err(NoteError::Decode(format!(
                            "{PROTOCOL_RLE}: high part followed by a pair with length {next_len:#04x} after {} pixels",
                            out.len()
                        )));
                    }
                    pairs.next();
                    (high + 1) * 128 + usize::from(next_len) + 1
                }
                // a high part alone, also as the last pair, is a run of (h + 1) * 128
                _ => (high + 1) * 128,
            }
        };
        let end = out.len().saturating_add(run);
        if end > expected {
            return Err(NoteError::Decode(format!(
                "{PROTOCOL_RLE}: expected {expected} pixels, the stream produces at least {end}"
            )));
        }
        out.resize(end, code);
    }
    if out.len() != expected {
        return Err(NoteError::Decode(format!(
            "{PROTOCOL_RLE}: expected {expected} pixels, got {}",
            out.len()
        )));
    }
    Ok(out)
}

/// Largest accepted bitmap side in pixels (the largest page is 2560; docs/note-format.md 4.6).
pub const MAX_SIDE: u32 = 16384;
/// Largest accepted pixel count (64 MPixel; a 1920 x 2560 page is under 5 MPixel).
pub const MAX_PIXELS: u64 = 64 * 1024 * 1024;

/// `width * height` after the plausibility checks; the sizes come from the file
/// (device code, `LINKRECT`, `TITLERECT`), so they are checked before any allocation
/// (docs/note-format.md 10).
pub(crate) fn pixel_count(what: &str, width: u32, height: u32) -> Result<usize, NoteError> {
    let pixels = u64::from(width) * u64::from(height);
    if width > MAX_SIDE || height > MAX_SIDE || pixels > MAX_PIXELS {
        return Err(NoteError::Decode(format!(
            "{what}: implausible bitmap size {width} x {height}"
        )));
    }
    usize::try_from(pixels).map_err(|_| {
        NoteError::Decode(format!(
            "{what}: bitmap size {width} x {height} is too large"
        ))
    })
}

/// Decodes a payload of the named protocol to a [`DecodedLayer`] of `width` x `height`.
fn decode_payload(
    what: &str,
    protocol: &str,
    data: &[u8],
    width: u32,
    height: u32,
    quirk_allowed: bool,
) -> Result<DecodedLayer, NoteError> {
    if protocol != PROTOCOL_RLE {
        let what = if data.starts_with(&[0x89, b'P', b'N', b'G']) {
            "PNG"
        } else {
            protocol
        };
        return Err(NoteError::Decode(format!(
            "unsupported bitmap protocol {what:?} (only {PROTOCOL_RLE} is decoded)"
        )));
    }
    let expected = pixel_count(what, width, height)?;
    let codes = match decode_rle(data, expected).map_err(|e| context(e, what)) {
        Ok(codes) => codes,
        Err(first) if quirk_allowed && data.len() == QUIRK_BLOCK_LEN => {
            decode_rle_with(data, expected, BLANK_RUN_QUIRK).map_err(|_| first)?
        }
        Err(e) => return Err(e),
    };
    Ok(DecodedLayer::new(width, height, codes))
}

impl Notebook {
    /// Decodes the bitmap of `layer` on `page` (docs/note-format.md 5, 6.1 step 1).
    pub fn decode_page_layer(&self, page: &Page, layer: &Layer) -> Result<DecodedLayer, NoteError> {
        let bitmap = layer.bitmap.ok_or_else(|| {
            NoteError::Decode(format!("layer {} has no bitmap (address 0)", layer.name))
        })?;
        let data = self.block(bitmap.address)?;
        // the 5134-byte quirk belongs to the style_white background (docs/note-format.md 5.2)
        let quirk = layer.slot == LayerSlot::Background;
        decode_payload(
            &format!("layer {}", layer.name),
            &layer.protocol,
            data,
            page.width,
            page.height,
            quirk,
        )
    }

    /// Decodes one layer of a page by its slot key (`MAINLAYER`, `LAYER1`..`LAYER3`,
    /// `BGLAYER`) or, failing that, its `LAYERNAME` (docs/note-format.md 4.5, 5).
    pub fn decode_layer(&self, page: usize, layer_name: &str) -> Result<DecodedLayer, NoteError> {
        let page = self
            .page(page)
            .ok_or_else(|| NoteError::Decode(format!("no page {page}")))?;
        let layer = page
            .layers
            .iter()
            .find(|l| l.name == layer_name)
            .or_else(|| {
                page.layers
                    .iter()
                    .find(|l| l.layer_name.as_deref() == Some(layer_name))
            })
            .ok_or_else(|| NoteError::Decode(format!("no layer {layer_name}")))?;
        self.decode_page_layer(page, layer)
    }

    /// Decodes a link bitmap: `LINKRECT` width x height pixels (docs/note-format.md 6.3, 7.3).
    pub fn decode_link_bitmap(&self, link: &Link) -> Result<DecodedLayer, NoteError> {
        let data = link
            .bitmap
            .as_deref()
            .ok_or_else(|| NoteError::Decode(format!("link {} has no bitmap", link.key)))?;
        let protocol = link.protocol.as_deref().unwrap_or(PROTOCOL_RLE);
        let [_, _, width, height] = link.rect;
        decode_payload(
            &format!("link {}", link.key),
            protocol,
            data,
            width,
            height,
            false,
        )
    }

    /// Decodes a title bitmap: `TITLERECT` width x height pixels (docs/note-format.md 6.3, 7.1;
    /// the size rule is inferred by analogy with links).
    pub fn decode_title_bitmap(&self, title: &Title) -> Result<DecodedLayer, NoteError> {
        let data = title
            .bitmap
            .as_deref()
            .ok_or_else(|| NoteError::Decode(format!("title {} has no bitmap", title.key)))?;
        let protocol = title.protocol.as_deref().unwrap_or(PROTOCOL_RLE);
        let [_, _, width, height] = title.rect;
        decode_payload(
            &format!("title {}", title.key),
            protocol,
            data,
            width,
            height,
            false,
        )
    }
}

fn context(error: NoteError, what: &str) -> NoteError {
    match error {
        NoteError::Decode(m) => NoteError::Decode(format!("{what}: {m}")),
        other => other,
    }
}

/// Gray level of a code in the X2 table (docs/note-format.md 5.3); `None` = transparent.
pub fn gray_x2(code: u8) -> Option<u8> {
    match code {
        0x62 => None,
        0x61 | 0x66 => Some(0),
        0x63 => Some(0x30),
        0x64 => Some(0x50),
        0x65 => Some(0xFE),
        0x9D | 0x9E => Some(0x9D),
        0xC9 | 0xCA => Some(0xC9),
        other => Some(other),
    }
}

/// Gray level of a code in the older X table (docs/note-format.md 5.3).
pub fn gray_x(code: u8) -> Option<u8> {
    match code {
        0x62 => None,
        0x61 | 0x66 => Some(0),
        0x63 | 0x67 => Some(0x9D),
        0x64 | 0x68 => Some(0xC9),
        0x65 => Some(0xFE),
        other => Some(other),
    }
}
