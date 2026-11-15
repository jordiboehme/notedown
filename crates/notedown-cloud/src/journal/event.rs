//! Journal events: one JSON object per line, fixed field order.
//!
//! The order is `seq`, `ts`, `account`, `origin`, `device` (when known), `type`, the fields of
//! the type, then `opId`. `seq` and `ts` are assigned by [`crate::journal::Journal::append`]
//! under the journal lock; a planned event carries `0` in both.

use serde::{Deserialize, Serialize};

use crate::store::RecycleReason;

/// Who caused a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// A request of a device.
    Device,
    /// A CLI command (`put`, `import`, `recycle prune`).
    Cli,
    /// The scan of out-of-band changes.
    Scan,
    /// Crash repair: the change was applied, but its events were missing from the journal.
    Repair,
    /// An event of `notedown cloud journal snapshot`: the state at that time, not a change.
    Snapshot,
    /// The hourly prune of the recycle bin by the server.
    Retention,
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
    /// The device reported another firmware `version` than the stored one.
    #[serde(rename = "device.firmware_changed", rename_all = "camelCase")]
    DeviceFirmwareChanged { from: String, to: String },
    /// A page with a new `PAGEID` (see [`crate::pages`]). `id` is the note's store id, `fileId`
    /// its `FILE_ID` (or `path:<md5 of the wire path>`), `position` 0-based.
    #[serde(rename = "page.added", rename_all = "camelCase")]
    PageAdded {
        id: u64,
        file_id: String,
        page_id: String,
        position: u64,
    },
    /// A page with the same `PAGEID` and other content; `parts` lists what changed.
    #[serde(rename = "page.modified", rename_all = "camelCase")]
    PageModified {
        id: u64,
        file_id: String,
        page_id: String,
        position: u64,
        parts: Vec<PartChange>,
    },
    /// A `PAGEID` that the note no longer has.
    #[serde(rename = "page.removed", rename_all = "camelCase")]
    PageRemoved {
        id: u64,
        file_id: String,
        page_id: String,
    },
    /// A page at another position (0-based).
    #[serde(rename = "page.moved", rename_all = "camelCase")]
    PageMoved {
        id: u64,
        file_id: String,
        page_id: String,
        from: u64,
        to: u64,
    },
    /// The note could not be indexed (it does not parse, or it is too large). The file event
    /// of the same commit stands; only the page events are missing.
    #[serde(rename = "page.index_failed", rename_all = "camelCase")]
    PageIndexFailed {
        id: u64,
        path: String,
        reason: String,
    },
    /// A new digest, or a deleted one that came back (see [`crate::appdata`]).
    #[serde(rename = "digest.created")]
    DigestCreated(DigestInfo),
    /// A changed digest.
    #[serde(rename = "digest.updated")]
    DigestUpdated(DigestInfo),
    /// A digest was soft-deleted; the record stays with `isDeleted: "Y"`.
    #[serde(rename = "digest.deleted")]
    DigestDeleted(DigestInfo),
    #[serde(rename = "digest_group.created")]
    DigestGroupCreated(DigestGroupInfo),
    #[serde(rename = "digest_group.updated")]
    DigestGroupUpdated(DigestGroupInfo),
    /// A digest group was soft-deleted.
    #[serde(rename = "digest_group.deleted")]
    DigestGroupDeleted(DigestGroupInfo),
    #[serde(rename = "task.created")]
    TaskCreated(TaskInfo),
    #[serde(rename = "task.updated")]
    TaskUpdated(TaskInfo),
    /// A to-do was deleted; the event keeps its title and status, the record is only a
    /// tombstone until it is swept.
    #[serde(rename = "task.deleted")]
    TaskDeleted(TaskInfo),
    #[serde(rename = "task_group.created")]
    TaskGroupCreated(TaskGroupInfo),
    #[serde(rename = "task_group.updated")]
    TaskGroupUpdated(TaskGroupInfo),
    #[serde(rename = "task_group.deleted")]
    TaskGroupDeleted(TaskGroupInfo),
    /// A line of the recycle bin index was removed by the retention, with its slot
    /// `recycle/<deletedAt>/<path>`. The fields are those of the line.
    #[serde(rename = "recycle.pruned", rename_all = "camelCase")]
    RecyclePruned {
        id: u64,
        path: String,
        md5: String,
        deleted_at: u64,
        reason: RecycleReason,
    },
    /// The account was removed (`cloud account remove`). The slug is in the event's `account`
    /// field. The event is appended before the account folder goes away, so a crash in between
    /// leaves a live account with this record: a reader must find the account missing too.
    #[serde(rename = "account.removed", rename_all = "camelCase")]
    AccountRemoved { user_id: u64 },
}

/// The fields of `digest.*` events. `uniqueIdentifier` is the digest's identity,
/// `metadata.unique_identifier` (the top-level field of the record is empty on items), or
/// empty when the metadata has none. `sourcePath` is empty and `sourceType` null when the
/// record has none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DigestInfo {
    pub id: u64,
    pub unique_identifier: String,
    pub source_path: String,
    #[serde(default)]
    pub source_type: Option<i64>,
    pub md5_hash: String,
}

/// The fields of `digest_group.*` events; `name` is empty when the group has none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DigestGroupInfo {
    pub id: u64,
    pub name: String,
}

/// The fields of `task.*` events; `title` and `status` are empty when the task has none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskInfo {
    pub task_id: String,
    pub task_list_id: String,
    pub title: String,
    pub status: String,
}

/// The fields of `task_group.*` events; `title` is empty when the group has none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskGroupInfo {
    pub task_list_id: String,
    pub title: String,
}

/// One changed part of a page in `page.modified`: its name (`strokes`, `layers`, `textboxes`,
/// `template`, `recognition`) and its hash before and after.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartChange {
    pub name: String,
    pub old: String,
    pub new: String,
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
            EventBody::DeviceFirmwareChanged { .. } => "device.firmware_changed",
            EventBody::PageAdded { .. } => "page.added",
            EventBody::PageModified { .. } => "page.modified",
            EventBody::PageRemoved { .. } => "page.removed",
            EventBody::PageMoved { .. } => "page.moved",
            EventBody::PageIndexFailed { .. } => "page.index_failed",
            EventBody::DigestCreated(_) => "digest.created",
            EventBody::DigestUpdated(_) => "digest.updated",
            EventBody::DigestDeleted(_) => "digest.deleted",
            EventBody::DigestGroupCreated(_) => "digest_group.created",
            EventBody::DigestGroupUpdated(_) => "digest_group.updated",
            EventBody::DigestGroupDeleted(_) => "digest_group.deleted",
            EventBody::TaskCreated(_) => "task.created",
            EventBody::TaskUpdated(_) => "task.updated",
            EventBody::TaskDeleted(_) => "task.deleted",
            EventBody::TaskGroupCreated(_) => "task_group.created",
            EventBody::TaskGroupUpdated(_) => "task_group.updated",
            EventBody::TaskGroupDeleted(_) => "task_group.deleted",
            EventBody::RecyclePruned { .. } => "recycle.pruned",
            EventBody::AccountRemoved { .. } => "account.removed",
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
