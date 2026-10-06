//! Text boxes and note digests (docs/note-format.md 7.4, 9.3, 9.5).
//!
//! A text box is a stroke record with kind name `0000` and operation 52
//! ([`RecordKind::TextBox`]). Its part P holds the text, font and digest
//! reference as base64 items (first string, same layout as a link box, 9.6) and
//! an embedded copy of the hidden source handwriting (third string,
//! `TEXTBOX_TRAILS`). The source strokes on the page keep status -5 and are
//! referenced by the group record of operation 52 ([`RecordKind::Group`]).

use serde::Serialize;
use serde_json::Value;

use crate::error::NoteError;
use crate::notebook::Notebook;
use crate::strokes::{Reader, RecordKind, StrokeRecord, capacity, parse_record};
use crate::values::parse_rect;

/// Magic at the start of the extension payload of a text box (7.4).
const TRAILS_MAGIC: &[u8] = b"TEXTBOX_TRAILS";

/// Items of the first string up to and including the style list (7.4); the
/// digest JSON, when present, is one more.
const MIN_ITEMS: usize = 14;

/// The digest a note digest registered for a text box (docs/note-format.md 7.4,
/// "Digest link"). The digest itself is not stored in the `.note` file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DigestRef {
    /// `digest_id`, 32 hex digits in the sample.
    pub id: String,
    /// `digest_time`, milliseconds since the epoch.
    pub time_ms: Option<u64>,
    /// `digest_source`.
    pub source: Option<String>,
}

/// A text box of a page (docs/note-format.md 7.4).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TextBox {
    /// 0-based page position.
    pub page: usize,
    /// 0-based position of the text box record in the page's `TOTALPATH`.
    pub record_index: usize,
    /// Record id (part I).
    pub id: u32,
    /// `left, top, width, height` from the record's rect item; when that item
    /// does not parse, from the extent of the outline ring.
    pub rect: [u32; 4],
    /// The text, UTF-8.
    pub text: String,
    /// Font size, `48.000000` in the sample.
    pub font_size: Option<f32>,
    /// Font path; `None` for `none`.
    pub font_path: Option<String>,
    /// Creation timestamp `YYYYMMDDhhmmss` + microseconds, local time, raw.
    pub created: Option<String>,
    /// The style list of 18 comma-separated numbers, raw (meaning unknown).
    pub style_raw: String,
    /// The items between the style list and the digest JSON, raw (`63`, `1`, `1`,
    /// `0` in the sample; meaning unknown).
    pub flags_raw: Vec<String>,
    /// The digest reference; `None` for a plain typed text box.
    pub digest: Option<DigestRef>,
    /// Ids of the hidden source strokes, from the group record that references
    /// this text box.
    pub source_stroke_ids: Vec<u32>,
    /// The decoded `TEXTBOX_TRAILS` copies of the source strokes (status 0).
    pub source_strokes: Vec<StrokeRecord>,
}

fn err(record: usize, message: impl std::fmt::Display) -> NoteError {
    NoteError::Decode(format!("text box record {record}: {message}"))
}

/// Decodes the text boxes among the records of page `page` (0-based).
pub fn decode_text_boxes(records: &[StrokeRecord], page: usize) -> Result<Vec<TextBox>, NoteError> {
    let mut boxes = Vec::new();
    for record in records.iter().filter(|r| r.kind() == RecordKind::TextBox) {
        boxes.push(decode_one(records, record, page)?);
    }
    Ok(boxes)
}

fn decode_one(
    records: &[StrokeRecord],
    record: &StrokeRecord,
    page: usize,
) -> Result<TextBox, NoteError> {
    let at = record.index;
    let trailer = record
        .trailer
        .as_ref()
        .ok_or_else(|| err(at, "part P does not parse"))?;
    let items = record
        .link_items()
        .filter(|items| items.len() >= MIN_ITEMS)
        .ok_or_else(|| {
            err(
                at,
                format!("fewer than {MIN_ITEMS} items in the first string"),
            )
        })?;
    let item = |n: usize| items.get(n).map(String::as_str).unwrap_or_default();

    // 7.4: the digest JSON is the last item when it is an object
    let digest_json = items
        .get(MIN_ITEMS..)
        .and_then(|rest| rest.last())
        .and_then(|last| serde_json::from_str::<Value>(last).ok())
        .filter(Value::is_object);
    let flags_end = items.len() - usize::from(digest_json.is_some());
    let flags_raw = items.get(MIN_ITEMS..flags_end).unwrap_or_default().to_vec();

    let rect = parse_rect(item(4)).unwrap_or_else(|| ring_rect(record));
    let created = Some(item(3).to_owned()).filter(|v| !v.is_empty() && v != "none");
    let font_path = Some(item(11).to_owned()).filter(|v| !v.is_empty() && v != "none");

    let source_stroke_ids = records
        .iter()
        .filter(|g| g.kind() == RecordKind::Group && g.referenced_ids.contains(&record.id))
        .flat_map(|g| g.referenced_ids.iter().copied())
        .filter(|&id| id != record.id)
        .fold(Vec::new(), |mut ids, id| {
            if !ids.contains(&id) {
                ids.push(id);
            }
            ids
        });

    let payload = trailer
        .strings
        .get(2)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let source_strokes =
        decode_trails(payload, (record.page_width, record.page_height)).map_err(|m| err(at, m))?;

    Ok(TextBox {
        page,
        record_index: at,
        id: record.id,
        rect,
        text: item(12).to_owned(),
        font_size: item(10).trim().parse().ok(),
        font_path,
        created,
        style_raw: item(13).to_owned(),
        flags_raw,
        digest: digest_json.as_ref().and_then(digest_ref),
        source_stroke_ids,
        source_strokes,
    })
}

/// The outline ring of a text box is its rect as a closed polygon (7.4); it is
/// one unit narrower than the rect item (256..526 for width 271).
fn ring_rect(record: &StrokeRecord) -> [u32; 4] {
    let points = record
        .rings
        .iter()
        .flat_map(|ring| ring.points.iter().copied());
    let (mut left, mut top) = (f32::MAX, f32::MAX);
    let (mut right, mut bottom) = (f32::MIN, f32::MIN);
    for (x, y) in points {
        left = left.min(x);
        top = top.min(y);
        right = right.max(x);
        bottom = bottom.max(y);
    }
    if left > right || top > bottom {
        return [0; 4];
    }
    [
        left.max(0.0) as u32,
        top.max(0.0) as u32,
        (right - left).max(0.0) as u32,
        (bottom - top).max(0.0) as u32,
    ]
}

fn digest_ref(json: &Value) -> Option<DigestRef> {
    let string = |key: &str| match json.get(key)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    Some(DigestRef {
        id: string("digest_id")?,
        time_ms: string("digest_time").and_then(|t| t.trim().parse().ok()),
        source: string("digest_source"),
    })
}

/// The `TEXTBOX_TRAILS` payload (7.4, 9.3 P): magic, `u32` count, `u32` 1000,
/// `u32` 4, then count x (`u32` length + stroke record). An empty payload (a
/// plain typed text box) or one without the magic gives no strokes.
fn decode_trails(payload: &[u8], size: (u32, u32)) -> Result<Vec<StrokeRecord>, String> {
    let Some(rest) = payload.strip_prefix(TRAILS_MAGIC) else {
        return Ok(Vec::new());
    };
    let mut r = Reader::new(rest);
    // each copy needs its 4-byte length and, at least, a record head
    let count = r.count(4 + 212, "TEXTBOX_TRAILS count")?;
    r.u32("TEXTBOX_TRAILS unknown")?;
    r.u32("TEXTBOX_TRAILS unknown")?;
    let mut out = Vec::with_capacity(capacity::<StrokeRecord>(count, 4 + 212));
    for index in 0..count {
        let len = r.count(1, "TEXTBOX_TRAILS record length")?;
        let body = r.take(len, "TEXTBOX_TRAILS record")?;
        out.push(
            parse_record(index, body, size)
                .map_err(|m| format!("TEXTBOX_TRAILS record {index}: {m}"))?,
        );
    }
    Ok(out)
}

impl Notebook {
    /// The text boxes of page `page` (0-based), in record order (docs/note-format.md
    /// 7.4). Pages without `TOTALPATH`, and files without text boxes, give an
    /// empty list. The text is also in the page bitmap, drawn by the device.
    pub fn text_boxes(&self, page: usize) -> Result<Vec<TextBox>, NoteError> {
        decode_text_boxes(&self.strokes(page)?, page)
    }
}
