//! Page fingerprints: one hash per part of a page, taken from the raw block bytes
//! (docs/note-format.md 4.3 to 4.5, 7.4, 8, 9.1), never from rendered pixels.
//!
//! Every hash is the lowercase hex SHA-256 cut to [`HASH_HEX_LEN`] characters ([`short_hash`]).
//! A part that hashes several values hashes each one as `u64` length + tag, `u64` length +
//! bytes, in a fixed order, so no two inputs give the same byte stream. An absent block hashes
//! as empty bytes.
//!
//! - `strokes`: the `TOTALPATH` payload (plain [`short_hash`], so it is the prefix of the SHA-256
//!   that docs/note-format.md 11 publishes).
//! - `layers`: per layer, by its page key (`MAINLAYER`, `LAYER1`..`LAYER3`, `BGLAYER`), the
//!   plain hash of its `LAYERBITMAP` payload.
//! - `template`: the page keys `PAGESTYLE`, `PAGESTYLEMD5` and `ORIENTATION`.
//! - `textboxes`: the raw text box records of `TOTALPATH` (kind `0000`, operation 52; their part
//!   P holds the text, the font and the digest reference), then the page keys `PAGETEXTBOX` (a
//!   number, absent = `0`) and `DISABLE` (the rects as `l,t,w,h|`, absent or `none` = empty).
//!   These two are compared parsed, because newer firmware adds `PAGETEXTBOX:0` and
//!   `DISABLE:none` to every page it saves.
//! - `recognition`: the `RECOGNTEXT` and `RECOGNFILE` payloads.
//! - `total`: the part hashes in the order of [`PARTS`] (`layers` as [`PageFingerprint::layers_hash`]).
//!   It leaves out `pageId` and `position`, so a moved page keeps its total.

use std::collections::{BTreeMap, HashMap};

use notedown_note::{BlockRef, Notebook, RecordKind, VersionPolicy, decode_totalpath};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Length of every hash in hex characters.
pub const HASH_HEX_LEN: usize = 16;

/// The part names, in the order of `parts` in `page.modified` and of the total hash.
pub const PARTS: [&str; 5] = ["strokes", "layers", "textboxes", "template", "recognition"];

/// The first [`HASH_HEX_LEN`] lowercase hex characters of the SHA-256 of `bytes`.
pub fn short_hash(bytes: &[u8]) -> String {
    cut(Sha256::digest(bytes).as_slice())
}

fn cut(digest: &[u8]) -> String {
    let mut hex = const_hex::encode(digest);
    hex.truncate(HASH_HEX_LEN);
    hex
}

/// A hash over tagged fields: `u64` length + tag, `u64` length + bytes per field.
struct Fields(Sha256);

impl Fields {
    fn new() -> Fields {
        Fields(Sha256::new())
    }

    fn field(&mut self, tag: &str, bytes: &[u8]) -> &mut Fields {
        self.0.update((tag.len() as u64).to_le_bytes());
        self.0.update(tag.as_bytes());
        self.0.update((bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
        self
    }

    fn finish(&mut self) -> String {
        cut(self.0.clone().finalize().as_slice())
    }
}

/// The fingerprint of one page, as stored in `pages/<key>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageFingerprint {
    /// `PAGEID`. A page without one gets `position:<n>`; the second page with the same id gets
    /// `<id>#2`, the third `<id>#3`, and so on.
    pub page_id: String,
    /// 0-based position in the note.
    pub position: usize,
    pub strokes: String,
    /// Layer page key to the hash of its bitmap.
    pub layers: BTreeMap<String, String>,
    pub template: String,
    pub textboxes: String,
    pub recognition: String,
    pub total: String,
}

impl PageFingerprint {
    /// A fingerprint from its part hashes; `total` is computed.
    pub fn new(
        page_id: &str,
        position: usize,
        strokes: &str,
        layers: BTreeMap<String, String>,
        template: &str,
        textboxes: &str,
        recognition: &str,
    ) -> PageFingerprint {
        let mut fp = PageFingerprint {
            page_id: page_id.to_string(),
            position,
            strokes: strokes.to_string(),
            layers,
            template: template.to_string(),
            textboxes: textboxes.to_string(),
            recognition: recognition.to_string(),
            total: String::new(),
        };
        let mut total = Fields::new();
        for (name, hash) in fp.parts() {
            total.field(name, hash.as_bytes());
        }
        fp.total = total.finish();
        fp
    }

    /// One hash over all layers: their names and hashes in name order.
    pub fn layers_hash(&self) -> String {
        let mut h = Fields::new();
        for (name, hash) in &self.layers {
            h.field(name, hash.as_bytes());
        }
        h.finish()
    }

    /// The part hashes in the order of [`PARTS`].
    pub fn parts(&self) -> [(&'static str, String); 5] {
        [
            (PARTS[0], self.strokes.clone()),
            (PARTS[1], self.layers_hash()),
            (PARTS[2], self.textboxes.clone()),
            (PARTS[3], self.template.clone()),
            (PARTS[4], self.recognition.clone()),
        ]
    }
}

/// The fingerprints of a whole note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteFingerprint {
    /// `FILE_ID` as written in the header (not checked here; see [`super::note_key`]).
    pub file_id: Option<String>,
    pub pages: Vec<PageFingerprint>,
}

/// Parse `bytes` as a `.note` and fingerprint every page. The error is a reason for people:
/// the parse error, or the page and block that could not be read.
pub fn fingerprint_note(bytes: &[u8]) -> Result<NoteFingerprint, String> {
    fingerprint_owned(bytes.to_vec())
}

/// [`fingerprint_note`] without a copy of the bytes.
pub(crate) fn fingerprint_owned(bytes: Vec<u8>) -> Result<NoteFingerprint, String> {
    // A newer firmware's signature must not stop the index: the known rules still apply.
    let nb = Notebook::from_bytes(bytes, VersionPolicy::AllowUnknown).map_err(|e| e.to_string())?;
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut pages = Vec::with_capacity(nb.pages.len());
    for page in &nb.pages {
        let number = page.index + 1;
        let block = |what: &str, r: Option<BlockRef>| -> Result<&[u8], String> {
            match r {
                Some(b) => nb
                    .block(b.address)
                    .map_err(|e| format!("page {number} {what}: {e}")),
                None => Ok(&[]),
            }
        };
        let meta = |key: &str| page.meta.get(key).unwrap_or_default().as_bytes();

        let totalpath = block("TOTALPATH", page.totalpath)?;
        let strokes = short_hash(totalpath);

        let mut layers = BTreeMap::new();
        for layer in &page.layers {
            let bitmap = block(&layer.name, layer.bitmap)?;
            layers.insert(layer.name.clone(), short_hash(bitmap));
        }

        let template = Fields::new()
            .field("PAGESTYLE", meta("PAGESTYLE"))
            .field("PAGESTYLEMD5", meta("PAGESTYLEMD5"))
            .field("ORIENTATION", meta("ORIENTATION"))
            .finish();

        let mut textboxes = Fields::new();
        for record in text_box_records(totalpath, (nb.width, nb.height))
            .map_err(|e| format!("page {number}: {e}"))?
        {
            textboxes.field("record", record);
        }
        // Newer firmware writes PAGETEXTBOX:0 and DISABLE:none on every page (4.3); an older
        // page without the keys means the same, so the values are compared parsed.
        let flag = page.page_text_box.unwrap_or(0).to_string();
        let rects: String = page
            .disabled_rects
            .iter()
            .map(|[l, t, w, h]| format!("{l},{t},{w},{h}|"))
            .collect();
        let textboxes = textboxes
            .field("PAGETEXTBOX", flag.as_bytes())
            .field("DISABLE", rects.as_bytes())
            .finish();

        let recognition = Fields::new()
            .field("RECOGNTEXT", block("RECOGNTEXT", page.recogn_text_block)?)
            .field("RECOGNFILE", block("RECOGNFILE", page.recogn_file)?)
            .finish();

        let base = page
            .id
            .clone()
            .unwrap_or_else(|| format!("position:{}", page.index));
        let n = seen.entry(base.clone()).or_insert(0);
        *n += 1;
        let page_id = if *n == 1 { base } else { format!("{base}#{n}") };
        pages.push(PageFingerprint::new(
            &page_id,
            page.index,
            &strokes,
            layers,
            &template,
            &textboxes,
            &recognition,
        ));
    }
    Ok(NoteFingerprint {
        file_id: nb.file_id.clone(),
        pages,
    })
}

/// The raw bytes of the text box records of a `TOTALPATH` payload (docs/note-format.md 7.4,
/// 9.1: `u32` count, then `u32` length + record). The records are decoded to find their kind;
/// a payload that does not decode is an error.
fn text_box_records(totalpath: &[u8], page_size: (u32, u32)) -> Result<Vec<&[u8]>, String> {
    if totalpath.is_empty() {
        return Ok(Vec::new());
    }
    let records = decode_totalpath(totalpath, page_size).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut at = 4usize;
    for record in &records {
        let start = at + 4;
        let end = start + record.len;
        if record.kind() == RecordKind::TextBox {
            let raw = totalpath
                .get(start..end)
                .ok_or_else(|| format!("TOTALPATH: record {} is out of bounds", record.index))?;
            out.push(raw);
        }
        at = end;
    }
    Ok(out)
}
