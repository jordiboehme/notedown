//! The data of the text store: blocks, log lines and note states (spec 2.1, 2.2).
//!
//! Every store file uses camelCase field names, as the log lines of spec 2.2 do.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::page::StrokeEntry;

/// One block of page text with a stable id (`b1`, `b2`, ...) per note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub id: String,
    pub label: String,
    /// `[left, top, right, bottom]` on Chandra's 0..1000 grid; `region::block_rect`
    /// gives page pixels.
    pub bbox: [u32; 4],
    pub text: String,
    /// The ids of old blocks this block replaced.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replaces: Vec<String>,
}

/// The change of a note, page or block in an answer (spec 5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    New,
    Edited,
    Deleted,
    Moved,
    Replaced,
    Unscoped,
}

/// The `change` of a log line: page changes, and `note.*` for a whole note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineChange {
    #[serde(rename = "new")]
    New,
    #[serde(rename = "edited")]
    Edited,
    #[serde(rename = "deleted")]
    Deleted,
    #[serde(rename = "moved")]
    Moved,
    #[serde(rename = "note.deleted")]
    NoteDeleted,
    #[serde(rename = "note.moved")]
    NoteMoved,
    #[serde(rename = "note.unscoped")]
    NoteUnscoped,
}

impl LineChange {
    /// True for the `note.*` changes, which concern a whole note and name no page.
    pub fn is_note_change(self) -> bool {
        matches!(
            self,
            LineChange::NoteDeleted | LineChange::NoteMoved | LineChange::NoteUnscoped
        )
    }
}

/// One line of `log.jsonl`: a page text version or a note change (spec 2.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogLine {
    pub seq: u64,
    /// Ms since the epoch.
    pub ts: u64,
    /// The note key.
    pub note: String,
    #[serde(rename = "pageId", default, skip_serializing_if = "Option::is_none")]
    pub page_id: Option<String>,
    #[serde(rename = "pageHash", default, skip_serializing_if = "Option::is_none")]
    pub page_hash: Option<String>,
    pub change: LineChange,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The changed parts of the page: `strokes`, `layers`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signal: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<Block>,
    /// The wire path (`note.moved`: the new one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(rename = "oldPath", default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    /// The 0-based page position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<usize>,
}

impl LogLine {
    /// A line of `change` for `note` with every other field empty; `seq` and `ts` are set by
    /// the append.
    pub fn new(note: &str, change: LineChange) -> LogLine {
        LogLine {
            seq: 0,
            ts: 0,
            note: note.to_string(),
            page_id: None,
            page_hash: None,
            change,
            model: None,
            signal: Vec::new(),
            blocks: Vec::new(),
            path: None,
            old_path: None,
            position: None,
        }
    }
}

/// The OCR state of a page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PageOcrState {
    Done,
    #[default]
    Pending,
    Failed,
    Truncated,
}

/// The state of one page of a note (spec 2.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageState {
    pub page_id: String,
    /// 0-based page position.
    pub position: usize,
    /// The page index `total` hash.
    #[serde(default)]
    pub page_hash: String,
    /// The page index `strokes` hash.
    #[serde(default)]
    pub strokes_hash: String,
    /// The page index `layers` hash.
    #[serde(default)]
    pub layers_hash: String,
    /// The `seq` of the current text version.
    #[serde(default)]
    pub seq: Option<u64>,
    #[serde(default)]
    pub state: PageOcrState,
    /// The strokes of the version the text was made from (spec 2.3).
    #[serde(default)]
    pub stroke_list: Vec<StrokeEntry>,
    #[serde(default)]
    pub blocks: Vec<Block>,
}

/// A link of a note. The field set is provisional: the metadata task fills and may extend it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MetaLink {
    /// 0-based page position of the link.
    pub page: usize,
    /// The device path or URL the link points to.
    pub target: String,
    /// The target note's key, when it is a note in the store.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// A heading marked on the device. Provisional, like [`MetaLink`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MetaTitle {
    /// 0-based page position.
    pub page: usize,
    /// `[x, y, width, height]` in page pixels.
    pub rect: [u32; 4],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<u32>,
}

/// Metadata read from the `.note` without OCR (spec 2.1). Provisional field types: the
/// metadata task fills them and may extend them; every field has a default, so saved states
/// stay readable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NoteMeta {
    pub keywords: Vec<String>,
    pub titles: Vec<MetaTitle>,
    pub links: Vec<MetaLink>,
    /// 0-based positions of starred pages.
    pub stars: Vec<usize>,
    /// The device's own recognition text per `pageId`.
    pub recognition: BTreeMap<String, String>,
    /// The ids of the digests that point to this note.
    pub digests: Vec<String>,
}

/// The current state of a note: `notes/<key>.json` (spec 2.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteState {
    pub key: String,
    /// The wire path now.
    pub path: String,
    /// The file name without `.note`.
    pub title: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub md5: String,
    #[serde(default)]
    pub meta: NoteMeta,
    /// In page order.
    #[serde(default)]
    pub pages: Vec<PageState>,
    #[serde(default)]
    pub deleted: bool,
    /// Set by a `note.unscoped` line: the note left the scope (spec 4.5). It is not deleted;
    /// the flag keeps the worker from writing the line twice and is cleared when the note
    /// enters the scope again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unscoped: bool,
    /// Per replaced block id the ids that replaced it.
    #[serde(default)]
    pub successors: BTreeMap<String, Vec<String>>,
    /// The number of the next block id (`b<next_block>`).
    #[serde(default)]
    pub next_block: u64,
    /// Every log line of this note up to this `seq` is in this state. Set by
    /// `TextStore::save_note`; crash repair applies only the lines after it.
    #[serde(default)]
    pub log_seq: u64,
}

/// The title of a note at `path`: the file name without `.note`.
pub fn title_of(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(".note").unwrap_or(name).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_line_reads_like_the_spec() {
        let mut line = LogLine::new("F2024", LineChange::Edited);
        line.seq = 17;
        line.ts = 1;
        line.page_id = Some("P1".into());
        line.blocks.push(Block {
            id: "b3".into(),
            label: "text".into(),
            bbox: [1, 2, 3, 4],
            text: "x".into(),
            replaces: Vec::new(),
        });
        let text = serde_json::to_string(&line).unwrap();
        assert_eq!(
            text,
            r#"{"seq":17,"ts":1,"note":"F2024","pageId":"P1","change":"edited","blocks":[{"id":"b3","label":"text","bbox":[1,2,3,4],"text":"x"}]}"#
        );
        assert_eq!(serde_json::from_str::<LogLine>(&text).unwrap(), line);
        let note = serde_json::to_string(&LogLine::new("k", LineChange::NoteUnscoped)).unwrap();
        assert!(note.contains(r#""change":"note.unscoped""#), "{note}");
    }

    #[test]
    fn title_drops_folder_and_suffix() {
        assert_eq!(title_of("NOTE/Note/Planview.note"), "Planview");
        assert_eq!(title_of("x"), "x");
    }
}
