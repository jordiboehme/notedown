//! Stroke lists of a note page: what the change detection compares.

use notedown_note::{Notebook, RecordKind, StrokeRecord, decode_text_boxes};
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

/// The live ink strokes and the text boxes of page `page` (0-based) of `nb`: what a change
/// region is made of (spec 3.1 point 2). See [`stroke_entries`].
pub fn stroke_list(nb: &Notebook, page: usize) -> Result<Vec<StrokeEntry>> {
    stroke_entries(&nb.strokes(page)?, page)
}

/// The stroke list of the TOTALPATH `records` of page `page`: every live ink stroke, then
/// every text box. Highlighter strokes are not filtered yet (spec 3.1 point 3 is open): every
/// live ink stroke counts.
///
/// The hash is the first 16 hex digits of SHA-256 over the canonical JSON of the parsed
/// record without its position `index` (the parser does not keep the raw record bytes). The
/// record holds its coordinates (and a text box its text), so a moved stroke or an edited text
/// box gets a new hash. A text box has the box of its rect. Each text box is decoded on its
/// own, so one that cannot be decoded is left out and the others stay.
pub fn stroke_entries(records: &[StrokeRecord], page: usize) -> Result<Vec<StrokeEntry>> {
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
    for rec in records.iter().filter(|r| r.kind() == RecordKind::TextBox) {
        // `decode_text_boxes` stops at the first text box it cannot decode, so it sees this
        // text box alone (with every other record, for its group).
        let alone: Vec<StrokeRecord> = records
            .iter()
            .filter(|r| r.kind() != RecordKind::TextBox || r.index == rec.index)
            .cloned()
            .collect();
        let Some(tb) = decode_text_boxes(&alone, page)
            .ok()
            .and_then(|b| b.into_iter().next())
        else {
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

    // A minimal TOTALPATH builder (docs/note-format.md 9.1, 9.3, 7.4), after
    // crates/notedown-note/tests/it/textboxes.rs.
    struct Rec {
        tool: u32,
        class: i32,
        kind: &'static str,
        operation: i32,
        id: u32,
        points: Vec<(u32, u32)>,
        ring: Vec<(f32, f32)>,
        first: Vec<u8>,
    }

    fn ink(id: u32) -> Rec {
        Rec {
            tool: 10,
            class: 5000,
            kind: "others",
            operation: 0,
            id,
            points: vec![(1404, 10347), (1500, 10300), (1600, 10200)],
            ring: vec![(10.0, 20.0), (30.0, 20.0), (30.0, 40.0), (10.0, 20.0)],
            first: b"none".to_vec(),
        }
    }

    /// A text box (7.4) with `text` and the rect `256,796,271,88`.
    fn text_box(id: u32, text: &str) -> Rec {
        use base64::Engine;
        let items = [
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
        ];
        let mut first = Vec::new();
        for item in items {
            first.extend_from_slice(
                base64::engine::general_purpose::STANDARD
                    .encode(item)
                    .as_bytes(),
            );
            first.push(b',');
        }
        Rec {
            tool: 0,
            class: 0,
            kind: "0000",
            operation: 52,
            id,
            points: vec![(0, 0); 5],
            ring: vec![
                (256.0, 883.0),
                (526.0, 883.0),
                (526.0, 796.0),
                (256.0, 796.0),
                (256.0, 883.0),
            ],
            first,
        }
    }

    impl Rec {
        fn bytes(&self) -> Vec<u8> {
            let mut o: Vec<u8> = Vec::new();
            let u = |o: &mut Vec<u8>, v: u32| o.extend_from_slice(&v.to_le_bytes());
            let i = |o: &mut Vec<u8>, v: i32| o.extend_from_slice(&v.to_le_bytes());
            let name = |o: &mut Vec<u8>, s: &str| {
                let mut field = [0u8; 52];
                field[..s.len()].copy_from_slice(s.as_bytes());
                o.extend_from_slice(&field);
            };
            u(&mut o, self.tool);
            u(&mut o, 0);
            u(&mut o, 400);
            for v in [10, 0, 32] {
                u(&mut o, v);
            }
            i(&mut o, -1);
            u(&mut o, 3);
            u(&mut o, 0);
            u(&mut o, 0);
            i(&mut o, self.class);
            u(&mut o, 0);
            name(&mut o, self.kind);
            for v in [0, 0, 0, 0, 0, 0, 2] {
                i(&mut o, v);
            }
            u(&mut o, 15819);
            u(&mut o, 11864);
            name(&mut o, "superNoteNote");
            u(&mut o, 1);
            for _ in 0..4 {
                u(&mut o, 0);
            }
            u(&mut o, 0);
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
            u(&mut o, 0);
            u(&mut o, 0);
            i(&mut o, 0); // status: live
            i(&mut o, self.operation);
            u(&mut o, self.id);
            u(&mut o, 0);
            u(&mut o, 0);
            for v in [0, 0, 1, 1, 0, 0, 1, 1] {
                i(&mut o, v);
            }
            u(&mut o, 0);
            o.extend_from_slice(&[0; 9]);
            o.push(1);
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
            i(&mut o, -1);
            o.extend_from_slice(&1.0f64.to_le_bytes());
            o.push(0);
            i(&mut o, 0);
            u(&mut o, 0);
            u(&mut o, 1404);
            u(&mut o, 1872);
            u(&mut o, 0);
            o.push(0);
            for s in [self.first.clone(), b"none".to_vec(), Vec::new()] {
                u(&mut o, s.len() as u32);
                o.extend_from_slice(&s);
            }
            u(&mut o, 3);
            u(&mut o, 0);
            o.extend_from_slice(&[0; 4]);
            o
        }
    }

    fn records(recs: &[Rec]) -> Vec<StrokeRecord> {
        let mut o = (recs.len() as u32).to_le_bytes().to_vec();
        for r in recs {
            let b = r.bytes();
            o.extend_from_slice(&(b.len() as u32).to_le_bytes());
            o.extend_from_slice(&b);
        }
        notedown_note::decode_totalpath(&o, (1404, 1872)).unwrap()
    }

    #[test]
    fn a_text_box_enters_the_list_and_the_region() {
        use crate::region::{Rect, Region, change_region};
        const BOX: [i32; 4] = [256, 796, 527, 884];
        let old = stroke_entries(&records(&[ink(1), text_box(2, "Hello")]), 0).unwrap();
        // (a) one ink stroke and one text box with the box of its rect.
        assert_eq!(old.len(), 2, "{old:?}");
        assert_eq!(old[1].bbox, BOX);
        assert_ne!(old[0].hash, old[1].hash);

        // (b) other text: another hash, the same box; the ink is unchanged.
        let new = stroke_entries(&records(&[ink(1), text_box(2, "Hello!")]), 0).unwrap();
        assert_eq!(new.len(), 2);
        assert_eq!(new[0], old[0]);
        assert_eq!(new[1].bbox, BOX);
        assert_ne!(new[1].hash, old[1].hash);

        // (c) the change region is the text box, not empty.
        let rect = Rect {
            x0: 256.0,
            y0: 796.0,
            x1: 527.0,
            y1: 884.0,
        };
        assert_eq!(
            change_region(&old, &new, &[]),
            Region::Parts(vec![rect, rect])
        );
    }

    #[test]
    fn a_text_box_that_does_not_decode_leaves_the_others() {
        let mut bad = text_box(3, "x");
        bad.first = b"bm9uZQ==,".to_vec(); // one item: too few for a text box
        let list = stroke_entries(&records(&[ink(1), bad, text_box(2, "Hello")]), 0).unwrap();
        assert_eq!(list.len(), 2, "{list:?}");
        assert_eq!(list[1].bbox, [256, 796, 527, 884]);
    }
}
