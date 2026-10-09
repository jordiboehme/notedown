//! Stroke lists of a note page: what the change detection compares.

use notedown_note::Notebook;
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

/// The live ink strokes of page `page` (0-based). Highlighter strokes are not
/// filtered yet (spec 3.1 point 3 is open): every live ink stroke counts.
///
/// The hash is the first 16 hex digits of SHA-256 over the canonical JSON of the
/// parsed record without its position `index` (the parser does not keep the raw
/// record bytes). The record holds its coordinates, so a moved stroke gets a new
/// hash.
pub fn stroke_list(nb: &Notebook, page: usize) -> Result<Vec<StrokeEntry>> {
    let mut out = Vec::new();
    for rec in nb.strokes(page)?.iter().filter(|r| r.is_live_ink()) {
        let mut value = serde_json::to_value(rec)?;
        if let Some(obj) = value.as_object_mut() {
            obj.remove("index");
        }
        let digest = Sha256::digest(value.to_string().as_bytes());
        let hash = const_hex::encode(&digest[..8]);
        let b = rec.bbox;
        out.push(StrokeEntry {
            hash,
            bbox: [b.left, b.top, b.right, b.bottom],
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
}
