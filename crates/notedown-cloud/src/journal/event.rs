//! Journal events: one JSON object per line, fixed field order.
//!
//! The order is `seq`, `ts`, `account`, `origin`, `device` (when known), `type`, the fields of
//! the type, then `opId`. `seq` and `ts` are assigned by [`crate::journal::Journal::append`]
//! under the journal lock; a planned event carries `0` in both.

use serde::{Deserialize, Serialize};

/// Who caused a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// A request of a device.
    Device,
    /// A CLI command (`put`, `import`, `journal snapshot`).
    Cli,
    /// The scan of out-of-band changes.
    Scan,
    /// Crash repair: the change was applied, but its events were missing from the journal.
    Repair,
}

/// The kind of a file, by its extension (case-insensitive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    Note,
    Document,
    Mark,
    Image,
    Other,
}

impl FileKind {
    /// `note` for `.note`; `document` for pdf, epub, doc, docx, txt, cbz, fb2, xps; `mark` for
    /// `.mark`; `image` for png, jpg, jpeg, gif, bmp, webp; `other` for everything else.
    pub fn of(name: &str) -> FileKind {
        let ext = match name.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => ext.to_ascii_lowercase(),
            _ => return FileKind::Other,
        };
        match ext.as_str() {
            "note" => FileKind::Note,
            "pdf" | "epub" | "doc" | "docx" | "txt" | "cbz" | "fb2" | "xps" => FileKind::Document,
            "mark" => FileKind::Mark,
            "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" => FileKind::Image,
            _ => FileKind::Other,
        }
    }
}

/// The type of an event and its own fields. Serialized with the `type` tag first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EventBody {
    #[serde(rename = "file.created", rename_all = "camelCase")]
    FileCreated {
        id: u64,
        path: String,
        kind: FileKind,
        md5: String,
        size: u64,
        /// Only for notes; set by the page indexer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pages_changed: Option<u64>,
    },
    #[serde(rename = "file.modified", rename_all = "camelCase")]
    FileModified {
        id: u64,
        path: String,
        kind: FileKind,
        md5: String,
        prev_md5: String,
        size: u64,
        /// Only for notes; set by the page indexer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pages_changed: Option<u64>,
    },
    #[serde(rename = "file.moved", rename_all = "camelCase")]
    FileMoved {
        id: u64,
        path: String,
        old_path: String,
    },
    #[serde(rename = "file.copied", rename_all = "camelCase")]
    FileCopied {
        id: u64,
        source_id: u64,
        path: String,
    },
    #[serde(rename = "file.deleted", rename_all = "camelCase")]
    FileDeleted {
        id: u64,
        path: String,
        /// Relative path below `recycle/`.
        recycle: String,
    },
    #[serde(rename = "folder.created", rename_all = "camelCase")]
    FolderCreated { id: u64, path: String },
    #[serde(rename = "folder.moved", rename_all = "camelCase")]
    FolderMoved {
        id: u64,
        path: String,
        old_path: String,
    },
    #[serde(rename = "folder.deleted", rename_all = "camelCase")]
    FolderDeleted { id: u64, path: String },
    /// The serial is in the event's `device` field.
    #[serde(rename = "device.bound")]
    DeviceBound,
    /// The serial is in the event's `device` field.
    #[serde(rename = "device.unbound")]
    DeviceUnbound,
}

impl EventBody {
    /// The `type` string (`file.created`, ...).
    pub fn type_name(&self) -> &'static str {
        match self {
            EventBody::FileCreated { .. } => "file.created",
            EventBody::FileModified { .. } => "file.modified",
            EventBody::FileMoved { .. } => "file.moved",
            EventBody::FileCopied { .. } => "file.copied",
            EventBody::FileDeleted { .. } => "file.deleted",
            EventBody::FolderCreated { .. } => "folder.created",
            EventBody::FolderMoved { .. } => "folder.moved",
            EventBody::FolderDeleted { .. } => "folder.deleted",
            EventBody::DeviceBound => "device.bound",
            EventBody::DeviceUnbound => "device.unbound",
        }
    }
}

/// One journal line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    /// Server-wide, strictly increasing, never reused. Assigned by `append`.
    pub seq: u64,
    /// Milliseconds since the epoch. Assigned by `append`.
    pub ts: u64,
    /// The account slug.
    pub account: String,
    pub origin: Origin,
    /// The device serial, when a device caused the change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(flatten)]
    pub body: EventBody,
    /// The operation this event belongs to; all events of one commit share it.
    pub op_id: String,
}

impl Event {
    /// A planned event (`seq` and `ts` are 0 until `append`).
    pub fn new(
        account: &str,
        origin: Origin,
        device: Option<String>,
        op_id: &str,
        body: EventBody,
    ) -> Event {
        Event {
            seq: 0,
            ts: 0,
            account: account.to_string(),
            origin,
            device,
            body,
            op_id: op_id.to_string(),
        }
    }

    /// The compact JSON line, without the newline.
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("events serialize")
    }
}

/// A fresh operation id: a random UUID (version 4), lowercase, hyphenated.
pub fn new_op_id() -> String {
    let mut b = [0u8; 16];
    rand::fill(&mut b[..]);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = const_hex::encode(b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}
