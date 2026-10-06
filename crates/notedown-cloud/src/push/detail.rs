//! The detail push (`push = "detail"`, D2): one `FILE-SYN` item per pending file or folder
//! event instead of one `STARTSYNC`, with the fields of section 4.5 of the protocol notes.
//!
//! Ids come from the event; the parent folder ids are looked up in the id registry of the store
//! (no store lock needed). A parent that is not known any more gives `null`, and so does every
//! field the event cannot fill.

use serde::Serialize;

use crate::journal::{Event, EventBody};
use crate::store::{Store, WirePath};

/// One item of the message. The field order is the order of the 4.5 example; `None` is `null`.
#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct Item {
    message_type: &'static str,
    equipment_no: String,
    file_type: &'static str,
    id: Option<u64>,
    new_id: Option<u64>,
    original_name: Option<String>,
    new_name: Option<String>,
    md5: Option<String>,
    size: Option<u64>,
    directory_id: Option<u64>,
    go_directory_id: Option<u64>,
    original_path: Option<String>,
    new_path: Option<String>,
    timestamp: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Message {
    code: &'static str,
    msg: &'static str,
    timestamp: u64,
    msg_type: &'static str,
    data: Vec<Item>,
}

/// The parsed parent of a wire path string: its registry id and its `NOTE/Note/` form.
fn parent_of(store: &Store, path: &str) -> crate::Result<(Option<u64>, String)> {
    let Some(parent) = WirePath::parse_named(path).ok().and_then(|p| p.parent()) else {
        return Ok((None, "/".to_string()));
    };
    let id = if parent.is_root() {
        None
    } else {
        store.id_of(&parent)?
    };
    let text = if parent.is_root() {
        "/".to_string()
    } else {
        format!("{}/", parent.display())
    };
    Ok((id, text))
}

fn name_of(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// The item of one event; `None` for an event that is no file or folder change.
fn item(event: &Event, store: &Store) -> crate::Result<Option<Item>> {
    let mut it = Item {
        equipment_no: event.device.clone().unwrap_or_else(|| "null-null".into()),
        file_type: "FILE",
        timestamp: event.ts,
        ..Item::default()
    };
    match &event.body {
        EventBody::FileCreated {
            id,
            path,
            md5,
            size,
            ..
        }
        | EventBody::FileModified {
            id,
            path,
            md5,
            size,
            ..
        } => {
            it.message_type = if matches!(event.body, EventBody::FileCreated { .. }) {
                "DOWNLOADFILE"
            } else {
                "MODIFYFILE"
            };
            it.md5 = Some(md5.clone());
            it.size = Some(*size);
            located(&mut it, store, *id, path)?;
        }
        EventBody::FileDeleted { id, path, .. } => {
            it.message_type = "DELETEFILE";
            located(&mut it, store, *id, path)?;
        }
        EventBody::FileMoved { id, path, old_path } => {
            it.message_type = "MOVEFILE";
            moved(&mut it, store, *id, path, old_path)?;
        }
        EventBody::FileCopied {
            id,
            source_id,
            path,
        } => {
            it.message_type = "COPYFILE";
            let (go, new_path) = parent_of(store, path)?;
            it.id = Some(*source_id);
            it.new_id = Some(*id);
            it.new_name = Some(name_of(path));
            it.go_directory_id = go;
            it.new_path = Some(new_path);
        }
        EventBody::FolderCreated { id, path } => {
            it.message_type = "ADDFOLDER";
            it.file_type = "FOLDE";
            located(&mut it, store, *id, path)?;
        }
        EventBody::FolderDeleted { id, path } => {
            it.message_type = "DELETEFOLDER";
            it.file_type = "FOLDE";
            located(&mut it, store, *id, path)?;
        }
        EventBody::FolderMoved { id, path, old_path } => {
            it.message_type = "MOVEFOLDER";
            it.file_type = "FOLDE";
            moved(&mut it, store, *id, path, old_path)?;
        }
        _ => return Ok(None),
    }
    Ok(Some(it))
}

/// `id`, `originalName`, `directoryId` and `originalPath` of an entry at `path`.
fn located(it: &mut Item, store: &Store, id: u64, path: &str) -> crate::Result<()> {
    let (dir, original_path) = parent_of(store, path)?;
    it.id = Some(id);
    it.original_name = Some(name_of(path));
    it.directory_id = dir;
    it.original_path = Some(original_path);
    Ok(())
}

/// The fields of a move: the old name and parent, the new name and parent.
fn moved(it: &mut Item, store: &Store, id: u64, path: &str, old_path: &str) -> crate::Result<()> {
    located(it, store, id, old_path)?;
    let (go, new_path) = parent_of(store, path)?;
    it.new_name = Some(name_of(path));
    it.go_directory_id = go;
    it.new_path = Some(new_path);
    Ok(())
}

/// The `FILE-SYN` message with one item per file or folder event, in journal order, as the JSON
/// string argument of `ServerMessage`. Every item carries the `equipmentNo` of its own event, so
/// the pending push's `cause` (the newest event's device) is not needed here. Fails only when
/// the id registry cannot be read.
pub fn detail_message(
    events: &[Event],
    _cause: Option<&str>,
    store: &Store,
    now_ms: u64,
) -> crate::Result<String> {
    let mut data = Vec::with_capacity(events.len());
    for event in events {
        data.extend(item(event, store)?);
    }
    let m = Message {
        code: "200",
        msg: "",
        timestamp: now_ms,
        msg_type: "FILE-SYN",
        data,
    };
    Ok(serde_json::to_string(&m).expect("message serializes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{FileKind, Origin};
    use crate::store::Kind;

    /// A store of account `acc` with `NOTE/Note` (id 1000, 1001) and `NOTE/Note/Work` (1002).
    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        crate::auth::Accounts::new(dir.path())
            .create("acc", "a@example.org", "secret")
            .unwrap();
        let store = Store::open(dir.path(), "acc").unwrap();
        {
            let g = store.lock().unwrap();
            for (p, k) in [
                ("NOTE", Kind::Folder),
                ("NOTE/Note", Kind::Folder),
                ("NOTE/Note/Work", Kind::Folder),
            ] {
                store
                    .assign(&g, &WirePath::parse_named(p).unwrap(), k)
                    .unwrap();
            }
        }
        (dir, store)
    }

    fn ev(device: Option<&str>, body: EventBody) -> Event {
        let mut e = Event::new("acc", Origin::Device, device.map(String::from), "op", body);
        e.seq = 1;
        e.ts = 1000;
        e
    }

    /// The message of `events` with the envelope cut off: the items as JSON text.
    fn items(store: &Store, events: &[Event]) -> String {
        let m = detail_message(events, None, store, 5).unwrap();
        let head = r#"{"code":"200","msg":"","timestamp":5,"msgType":"FILE-SYN","data":["#;
        m.strip_prefix(head)
            .unwrap()
            .strip_suffix("]}")
            .unwrap()
            .to_string()
    }

    fn created() -> EventBody {
        EventBody::FileCreated {
            id: 4711,
            path: "NOTE/Note/Work/a.pdf".into(),
            kind: FileKind::Document,
            md5: "m5".into(),
            size: 12,
            pages_changed: None,
        }
    }

    #[test]
    fn file_created_is_downloadfile() {
        let (_d, s) = store();
        assert_eq!(
            items(&s, &[ev(Some("SN1"), created())]),
            r#"{"messageType":"DOWNLOADFILE","equipmentNo":"SN1","fileType":"FILE","id":4711,"newId":null,"originalName":"a.pdf","newName":null,"md5":"m5","size":12,"directoryId":1002,"goDirectoryId":null,"originalPath":"NOTE/Note/Work/","newPath":null,"timestamp":1000}"#
        );
    }

    #[test]
    fn file_modified_is_modifyfile() {
        let (_d, s) = store();
        let body = EventBody::FileModified {
            id: 4711,
            path: "NOTE/Note/b.note".into(),
            kind: FileKind::Note,
            md5: "m6".into(),
            prev_md5: "m5".into(),
            size: 20,
            pages_changed: None,
        };
        assert_eq!(
            items(&s, &[ev(None, body)]),
            r#"{"messageType":"MODIFYFILE","equipmentNo":"null-null","fileType":"FILE","id":4711,"newId":null,"originalName":"b.note","newName":null,"md5":"m6","size":20,"directoryId":1001,"goDirectoryId":null,"originalPath":"NOTE/Note/","newPath":null,"timestamp":1000}"#
        );
    }

    #[test]
    fn file_moved_is_movefile() {
        let (_d, s) = store();
        let body = EventBody::FileMoved {
            id: 4711,
            path: "NOTE/Note/Work/c.note".into(),
            old_path: "NOTE/Note/b.note".into(),
        };
        assert_eq!(
            items(&s, &[ev(None, body)]),
            r#"{"messageType":"MOVEFILE","equipmentNo":"null-null","fileType":"FILE","id":4711,"newId":null,"originalName":"b.note","newName":"c.note","md5":null,"size":null,"directoryId":1001,"goDirectoryId":1002,"originalPath":"NOTE/Note/","newPath":"NOTE/Note/Work/","timestamp":1000}"#
        );
    }

    #[test]
    fn file_copied_is_copyfile() {
        let (_d, s) = store();
        let body = EventBody::FileCopied {
            id: 4712,
            source_id: 4711,
            path: "NOTE/Note/Work/d.note".into(),
        };
        assert_eq!(
            items(&s, &[ev(None, body)]),
            r#"{"messageType":"COPYFILE","equipmentNo":"null-null","fileType":"FILE","id":4711,"newId":4712,"originalName":null,"newName":"d.note","md5":null,"size":null,"directoryId":null,"goDirectoryId":1002,"originalPath":null,"newPath":"NOTE/Note/Work/","timestamp":1000}"#
        );
    }

    #[test]
    fn file_deleted_is_deletefile() {
        let (_d, s) = store();
        let body = EventBody::FileDeleted {
            id: 4711,
            path: "NOTE/Note/e.note".into(),
            recycle: "r/e.note".into(),
        };
        assert_eq!(
            items(&s, &[ev(Some("SN2"), body)]),
            r#"{"messageType":"DELETEFILE","equipmentNo":"SN2","fileType":"FILE","id":4711,"newId":null,"originalName":"e.note","newName":null,"md5":null,"size":null,"directoryId":1001,"goDirectoryId":null,"originalPath":"NOTE/Note/","newPath":null,"timestamp":1000}"#
        );
    }

    #[test]
    fn folder_events_use_folde() {
        let (_d, s) = store();
        let added = EventBody::FolderCreated {
            id: 77,
            path: "NOTE/Note/New".into(),
        };
        let moved = EventBody::FolderMoved {
            id: 77,
            path: "NOTE/Note/Work/New".into(),
            old_path: "NOTE/Note/New".into(),
        };
        let deleted = EventBody::FolderDeleted {
            id: 77,
            path: "NOTE/Note/Work/New".into(),
        };
        let out = items(&s, &[ev(None, added), ev(None, moved), ev(None, deleted)]);
        assert_eq!(
            out,
            [
                r#"{"messageType":"ADDFOLDER","equipmentNo":"null-null","fileType":"FOLDE","id":77,"newId":null,"originalName":"New","newName":null,"md5":null,"size":null,"directoryId":1001,"goDirectoryId":null,"originalPath":"NOTE/Note/","newPath":null,"timestamp":1000}"#,
                r#"{"messageType":"MOVEFOLDER","equipmentNo":"null-null","fileType":"FOLDE","id":77,"newId":null,"originalName":"New","newName":"New","md5":null,"size":null,"directoryId":1001,"goDirectoryId":1002,"originalPath":"NOTE/Note/","newPath":"NOTE/Note/Work/","timestamp":1000}"#,
                r#"{"messageType":"DELETEFOLDER","equipmentNo":"null-null","fileType":"FOLDE","id":77,"newId":null,"originalName":"New","newName":null,"md5":null,"size":null,"directoryId":1002,"goDirectoryId":null,"originalPath":"NOTE/Note/Work/","newPath":null,"timestamp":1000}"#,
            ]
            .join(",")
        );
    }

    #[test]
    fn a_burst_gives_one_item_per_event_in_order() {
        let (_d, s) = store();
        let events: Vec<Event> = ["a", "b", "c"]
            .iter()
            .map(|n| {
                ev(
                    None,
                    EventBody::FileCreated {
                        id: 1,
                        path: format!("NOTE/Note/{n}.note"),
                        kind: FileKind::Note,
                        md5: "m".into(),
                        size: 1,
                        pages_changed: None,
                    },
                )
            })
            .collect();
        let m: serde_json::Value =
            serde_json::from_str(&detail_message(&events, None, &s, 5).unwrap()).unwrap();
        let names: Vec<&str> = m["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["originalName"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["a.note", "b.note", "c.note"]);
    }

    #[test]
    fn an_unknown_parent_gives_null() {
        let (_d, s) = store();
        let body = EventBody::FileCreated {
            id: 1,
            path: "NOTE/Gone/a.note".into(),
            kind: FileKind::Note,
            md5: "m".into(),
            size: 1,
            pages_changed: None,
        };
        assert!(items(&s, &[ev(None, body)]).contains(r#""directoryId":null"#));
    }
}
