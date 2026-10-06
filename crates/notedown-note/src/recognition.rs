//! Real-time recognition data (docs/note-format.md 8): the `RECOGNTEXT` block
//! (8.2) and the `RECOGNFILE` ZIP archive with its `ink.bink` strokes (8.3).
//! Archive entries other than `ink.bink` are kept as raw bytes.

use std::io::{Cursor, Read};

use base64::Engine;
use serde::Serialize;
use serde_json::Value;

use crate::error::NoteError;
use crate::notebook::Notebook;
use crate::strokes::{Reader, capacity};

/// Largest decompressed size accepted per archive entry (10.9). t1's largest
/// entry is 72435 bytes.
const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
/// Ceiling of the total decompressed size of all entries of one archive (10.9).
const MAX_TOTAL_BYTES: u64 = 128 * 1024 * 1024;
/// Total budget per byte of the archive block: t1 inflates 27252 bytes to
/// 117522 (about 4.3 x).
const BUDGET_PER_BLOCK_BYTE: u64 = 64;
/// Floor of the total budget, so small archives are not cut short.
const MIN_TOTAL_BYTES: u64 = 1024 * 1024;
/// Largest number of archive entries read (10.9).
const MAX_ENTRIES: usize = 10_000;
/// Offset of the `1000, 1000, 3` parameters in the 4-channel `ink.bink`
/// header (8.3).
const BINK_PARAMS_AT: usize = 160;

/// Recognition data of one page (docs/note-format.md 8, Appendix A "Recognition").
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Recognition {
    /// Decoded `RECOGNTEXT` block (8.2), when the page has one.
    pub text: Option<RecognText>,
    /// Decoded `RECOGNFILE` archive (8.3), when the page has one.
    pub archive: Option<RecognArchive>,
}

/// Decoded `RECOGNTEXT` (docs/note-format.md 8.2).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecognText {
    /// The whole JSON document.
    pub json: Value,
    /// The entries of the `elements` array.
    pub elements: Vec<RecognElement>,
    /// The `label` of every `"type": "Text"` element, joined with `\n`. The spec
    /// does not say how elements are joined; one line per element is assumed.
    pub text: String,
}

/// One entry of the `elements` array (docs/note-format.md 8.2).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecognElement {
    /// `type`, e.g. `Text`.
    pub kind: Option<String>,
    /// `label`, the recognized text of a `Text` element.
    pub label: Option<String>,
    /// `words`; a word with label `\n` marks a line break \[others\].
    pub words: Vec<RecognWord>,
    /// The whole JSON object.
    pub raw: Value,
}

/// One recognized word (docs/note-format.md 8.2).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecognWord {
    pub label: String,
    /// `bounding-box`; its units are not page pixels \[others\].
    pub bounding_box: Option<WordBox>,
}

/// A word's `bounding-box` (docs/note-format.md 8.2), units unverified.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct WordBox {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// The `RECOGNFILE` ZIP archive (docs/note-format.md 8.3).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecognArchive {
    /// Entries in archive order.
    pub entries: Vec<ArchiveEntry>,
    /// The first `ink.bink` entry, decoded.
    pub ink: Option<InkBink>,
    /// Why the `ink.bink` entry could not be decoded; its bytes stay in `entries`.
    pub ink_error: Option<String>,
}

/// One archive entry (docs/note-format.md 8.3).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ArchiveEntry {
    /// Name as stored, e.g. `pages/<id>/ink.bink`. Entries are never written to
    /// disk, so names are not sanitized.
    pub name: String,
    /// Uncompressed size declared in the archive.
    pub size: u64,
    /// Compressed size declared in the archive.
    pub compressed_size: u64,
    /// Decompressed bytes; empty when `error` is set.
    #[serde(skip)]
    pub data: Vec<u8>,
    /// Why the entry could not be read (unsupported method, too large, ...).
    pub error: Option<String>,
}

/// Decoded `ink.bink` (docs/note-format.md 8.3).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InkBink {
    /// Bytes 0 to 160: magic `BINK` and the channel declarations (`X`, `Y` mm,
    /// `F`, `T` ms), raw.
    #[serde(skip)]
    pub header: Vec<u8>,
    /// The three `u32` at byte 160: `1000, 1000, 3`.
    pub params: [u32; 3],
    /// The `u8` after them (0).
    pub flag: u8,
    pub strokes: Vec<InkStroke>,
    /// The element table (recognition tree) after the strokes, raw.
    #[serde(skip)]
    pub rest: Vec<u8>,
}

/// One captured stroke of `ink.bink` (docs/note-format.md 8.3). Strokes are in
/// the same order as the `TOTALPATH` records with the same point counts.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InkStroke {
    /// The leading `u32` (0).
    pub reserved: u32,
    /// Microseconds since the Unix epoch, UTC.
    pub timestamp_us: u64,
    /// The duration-like `u32` (3145 for every t1 stroke).
    pub duration: u32,
    /// Points in millimeters, x then y.
    pub points_mm: Vec<(f32, f32)>,
    /// Force per point (all 0 in t1).
    pub force: Vec<f32>,
    /// Sample index per point (0..n-1 in t1).
    pub sample_index: Vec<u32>,
}

/// Decodes a `RECOGNTEXT` block payload: base64 of UTF-8 JSON
/// (docs/note-format.md 8.2, 10.10).
pub fn decode_recogn_text(data: &[u8]) -> Result<RecognText, NoteError> {
    let err = |m: String| NoteError::Decode(format!("RECOGNTEXT: {m}"));
    let compact: Vec<u8> = data
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&compact)
        .map_err(|e| err(format!("base64: {e}")))?;
    let json: Value = serde_json::from_slice(&bytes).map_err(|e| err(format!("JSON: {e}")))?;
    let elements: Vec<RecognElement> = json
        .get("elements")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(element).collect())
        .unwrap_or_default();
    let text = elements
        .iter()
        .filter(|e| e.kind.as_deref() == Some("Text"))
        .filter_map(|e| e.label.as_deref())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(RecognText {
        json,
        elements,
        text,
    })
}

fn string(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn element(v: &Value) -> RecognElement {
    let words = v
        .get("words")
        .and_then(Value::as_array)
        .map(|ws| {
            ws.iter()
                .map(|w| RecognWord {
                    label: string(w, "label").unwrap_or_default(),
                    bounding_box: w.get("bounding-box").and_then(word_box),
                })
                .collect()
        })
        .unwrap_or_default();
    RecognElement {
        kind: string(v, "type"),
        label: string(v, "label"),
        words,
        raw: v.clone(),
    }
}

fn word_box(v: &Value) -> Option<WordBox> {
    let f = |k: &str| v.get(k).and_then(Value::as_f64);
    Some(WordBox {
        x: f("x")?,
        y: f("y")?,
        width: f("width")?,
        height: f("height")?,
    })
}

/// The total decompression budget of an archive block of `len` bytes:
/// 64 x the block length, at least 1 MiB, at most 128 MiB (10.9).
fn total_budget(len: usize) -> u64 {
    let len = u64::try_from(len).unwrap_or(u64::MAX);
    len.saturating_mul(BUDGET_PER_BLOCK_BYTE)
        .clamp(MIN_TOTAL_BYTES, MAX_TOTAL_BYTES)
}

/// Decodes a `RECOGNFILE` block payload, a ZIP archive (docs/note-format.md
/// 8.3, 10.9). A damaged archive, or one whose entries share compressed data
/// (overlapping entries, a cheap zip bomb), is [`NoteError::Decode`]. All
/// entries together may inflate to at most 64 x the block length (at least
/// 1 MiB, at most 128 MiB): an entry
/// whose declared size does not fit the rest of the budget is not read, and a
/// `take` guard stops an entry whose header lies. Such an entry, and one that
/// cannot be read for another reason, carries its own `error` and no data. An
/// `ink.bink` that does not decode sets `ink_error`.
pub fn decode_recogn_file(data: &[u8]) -> Result<RecognArchive, NoteError> {
    let err = |m: String| NoteError::Decode(format!("RECOGNFILE: {m}"));
    let mut zip = zip::ZipArchive::new(Cursor::new(data)).map_err(|e| err(e.to_string()))?;
    if zip.len() > MAX_ENTRIES {
        return Err(err(format!("{} entries", zip.len())));
    }
    if zip
        .has_overlapping_files()
        .map_err(|e| err(e.to_string()))?
    {
        return Err(err("entries overlap (shared compressed data)".to_owned()));
    }
    let mut budget = total_budget(data.len());
    let mut entries = Vec::with_capacity(zip.len());
    for i in 0..zip.len() {
        let mut file = zip
            .by_index(i)
            .map_err(|e| err(format!("entry {i}: {e}")))?;
        let name = file.name().to_owned();
        let size = file.size();
        let compressed_size = file.compressed_size();
        let limit = budget.min(MAX_ENTRY_BYTES);
        let mut buf = Vec::new();
        let error = if size > limit {
            Some(format!(
                "declared size {size} exceeds the remaining budget of {limit} bytes"
            ))
        } else {
            let read = (&mut file).take(limit + 1).read_to_end(&mut buf);
            let got = u64::try_from(buf.len()).unwrap_or(u64::MAX);
            budget = budget.saturating_sub(got.min(limit));
            match read {
                Err(e) => Some(e.to_string()),
                Ok(_) if got > limit => Some(format!(
                    "inflates past the remaining budget of {limit} bytes"
                )),
                Ok(_) => None,
            }
        };
        if error.is_some() {
            buf = Vec::new();
        }
        entries.push(ArchiveEntry {
            name,
            size,
            compressed_size,
            data: buf,
            error,
        });
    }
    let (ink, ink_error) = match entries
        .iter()
        .find(|e| e.error.is_none() && e.name.ends_with("ink.bink"))
    {
        Some(entry) => match decode_ink_bink(&entry.data) {
            Ok(ink) => (Some(ink), None),
            Err(e) => (None, Some(e.to_string())),
        },
        None => (None, None),
    };
    Ok(RecognArchive {
        entries,
        ink,
        ink_error,
    })
}

/// Decodes an `ink.bink` entry (docs/note-format.md 8.3). Only the 4-channel
/// header seen in the samples is understood: the parameters at byte 160 must
/// read `1000, 1000, 3`.
pub fn decode_ink_bink(data: &[u8]) -> Result<InkBink, NoteError> {
    let err = |m: String| NoteError::Decode(format!("ink.bink: {m}"));
    if !data.starts_with(b"BINK") {
        return Err(err("no BINK magic".to_owned()));
    }
    let mut r = Reader::new(data);
    let header = r.take(BINK_PARAMS_AT, "header").map_err(err)?.to_vec();
    let params = [
        r.u32("params").map_err(err)?,
        r.u32("params").map_err(err)?,
        r.u32("params").map_err(err)?,
    ];
    if params != [1000, 1000, 3] {
        return Err(err(format!("unknown header parameters {params:?}")));
    }
    let flag = r.u8("flag").map_err(err)?;
    // each stroke needs at least 20 bytes
    let count = r.count(20, "stroke count").map_err(err)?;
    let mut strokes = Vec::with_capacity(capacity::<InkStroke>(count, 20));
    for i in 0..count {
        strokes.push(ink_stroke(&mut r).map_err(|m| err(format!("stroke {i}: {m}")))?);
    }
    Ok(InkBink {
        header,
        params,
        flag,
        strokes,
        rest: r.rest().to_vec(),
    })
}

fn ink_stroke(r: &mut Reader<'_>) -> Result<InkStroke, String> {
    let reserved = r.u32("reserved")?;
    let timestamp_us = r.u64("timestamp")?;
    let duration = r.u32("duration")?;
    // x, y, force and sample index: 16 bytes per point
    let n = r.count(16, "point count")?;
    let mut points_mm = Vec::with_capacity(n);
    for _ in 0..n {
        points_mm.push((r.f32("x")?, r.f32("y")?));
    }
    let mut force = Vec::with_capacity(n);
    for _ in 0..n {
        force.push(r.f32("force")?);
    }
    let mut sample_index = Vec::with_capacity(n);
    for _ in 0..n {
        sample_index.push(r.u32("sample index")?);
    }
    Ok(InkStroke {
        reserved,
        timestamp_us,
        duration,
        points_mm,
        force,
        sample_index,
    })
}

impl Notebook {
    /// The recognition data of page `page` (0-based): `RECOGNTEXT` and
    /// `RECOGNFILE` decoded (docs/note-format.md 8). `None` when the page has
    /// neither.
    pub fn recognition(&self, page: usize) -> Result<Option<Recognition>, NoteError> {
        let p = self.page_or_err(page)?;
        if p.recogn_text_block.is_none() && p.recogn_file.is_none() {
            return Ok(None);
        }
        let text = match p.recogn_text_block {
            Some(b) => Some(decode_recogn_text(self.block(b.address)?)?),
            None => None,
        };
        let archive = match p.recogn_file {
            Some(b) => Some(decode_recogn_file(self.block(b.address)?)?),
            None => None,
        };
        Ok(Some(Recognition { text, archive }))
    }
}
