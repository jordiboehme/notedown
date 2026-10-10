//! Stroke lists of a note page: what the change detection compares.

use notedown_note::{Notebook, StrokeRecord, decode_text_boxes};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Result;

/// One live ink stroke: a short hash and its box in page pixels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrokeEntry {
    pub hash: String,
    /// `[left, top, right, bottom]` in page pixels.
    pub bbox: [i32; 4],
}

/// The live ink strokes and the text boxes of page `page` (0-based): what a change region is
/// made of (spec 3.1 point 2). Highlighter strokes are not filtered yet (spec 3.1 point 3 is
/// open): every live ink stroke counts.
///
/// The hash is the first 16 hex digits of SHA-256 over the canonical JSON of the parsed
/// record without its position `index` (the parser does not keep the raw record bytes). The
/// record holds its coordinates (and a text box its text), so a moved stroke or an edited text
/// box gets a new hash. A text box has the box of its rect; text boxes that cannot be decoded
/// are left out.
pub fn stroke_list(nb: &Notebook, page: usize) -> Result<Vec<StrokeEntry>> {
    let records = nb.strokes(page)?;
    let hash = |rec: &StrokeRecord| -> Result<String> {
        let mut value = serde_json::to_value(rec)?;
        if let Some(obj) = value.as_object_mut() {
            obj.remove("index");
        }
        let digest = Sha256::digest(value.to_string().as_bytes());
        Ok(const_hex::encode(&digest[..8]))
    };
    let mut out = Vec::new();
    for rec in records.iter().filter(|r| r.is_live_ink()) {
        let b = rec.bbox;
        out.push(StrokeEntry {
            hash: hash(rec)?,
            bbox: [b.left, b.top, b.right, b.bottom],
        });
    }
    for tb in decode_text_boxes(&records, page).unwrap_or_default() {
        let Some(rec) = records.iter().find(|r| r.index == tb.record_index) else {
            continue;
        };
        let [l, t, w, h] = tb.rect.map(|v| i32::try_from(v).unwrap_or(i32::MAX));
        out.push(StrokeEntry {
            hash: hash(rec)?,
            bbox: [l, t, l.saturating_add(w), t.saturating_add(h)],
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn stroke_list_of_a_fixture_page() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/notes/test1.note");
        let bytes = std::fs::read(path).unwrap();
        let nb = Notebook::from_bytes(bytes, notedown_note::VersionPolicy::Strict).unwrap();
        let list = stroke_list(&nb, 0).unwrap();
        assert!(!list.is_empty());
        let (w, h) = (nb.pages[0].width as i32, nb.pages[0].height as i32);
        for e in &list {
            assert_eq!(e.hash.len(), 16);
            let [l, t, r, b] = e.bbox;
            assert!(
                l >= 0 && t >= 0 && r <= w && b <= h && l <= r && t <= b,
                "{e:?}"
            );
        }
    }

    #[test]
    fn a_text_box_is_in_the_stroke_list() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/local/bike-digest.note");
        if !path.exists() {
            eprintln!("skipping: local fixture {} is missing", path.display());
            return;
        }
        let nb = Notebook::open(&path, notedown_note::VersionPolicy::AllowUnknown).unwrap();
        let records = nb.strokes(2).unwrap();
        let boxes = decode_text_boxes(&records, 2).unwrap();
        assert!(!boxes.is_empty(), "page 3 of bike-digest has a text box");
        let list = stroke_list(&nb, 2).unwrap();
        let ink = records.iter().filter(|r| r.is_live_ink()).count();
        assert_eq!(list.len(), ink + boxes.len());
        let [l, t, w, h] = boxes[0].rect.map(|v| v as i32);
        assert!(
            list.iter().any(|e| e.bbox == [l, t, l + w, t + h]),
            "{list:?}"
        );
    }
}
