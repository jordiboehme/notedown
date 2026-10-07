//! The detail push (`push = "detail"`, D2): one `FILE-SYN` item per pending file or folder
//! event instead of one `STARTSYNC`, with the fields of section 4.5 of the protocol notes.
//!
//! Ids come from the event; the parent folder ids are looked up in the id registry of the store
//! (no store lock needed). A field the event cannot fill, or a parent that is not known any
//! more, is left out: Ratta's items never carry `null`.

use serde::Serialize;

use super::PushItem;
use crate::journal::{Event, EventBody};
use crate::store::{Store, WirePath};

/// The `equipmentNo` of a change on the server side: no device call and no channel (4.5).
const SERVER_CHANGE: &str = "null-null";

/// One item. The field order is the order of the 4.5 example; `None` is left out.
#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct Item {
    message_type: &'static str,
    equipment_no: &'static str,
    file_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    original_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    md5: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    directory_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    go_directory_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    original_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_path: Option<String>,
    timestamp: u64,
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

/// The item of one event; `None` for an event that is no file or folder change. Every item
/// carries `"null-null"`: the change happened on the server side for the device that gets it.
/// Its `timestamp` is `built_ms`, the time the item is built (4.5).
fn item(event: &Event, store: &Store, built_ms: u64) -> crate::Result<Option<Item>> {
    let mut it = Item {
        equipment_no: SERVER_CHANGE,
        file_type: "FILE",
        timestamp: built_ms,
        ..Item::default()
    };
    match &event.body {
        // A new file and a changed file are both downloaded (4.7).
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
            it.message_type = "DOWNLOADFILE";
            located(&mut it, store, *id, path)?;
            it.new_name = it.original_name.clone();
            it.new_path = it.original_path.clone();
            it.md5 = Some(md5.clone());
            it.size = Some(*size);
        }
        EventBody::FileDeleted { id, path, .. } => {
            it.message_type = "DELETEFILE";
            located(&mut it, store, *id, path)?;
        }
        EventBody::FileMoved { id, path, old_path } => {
            it.message_type = moved(
                &mut it,
                store,
                *id,
                path,
                old_path,
                "MODIFYFILE",
                "MOVEFILE",
            )?;
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
            it.size = Some(0);
        }
        EventBody::FolderDeleted { id, path } => {
            it.message_type = "DELETEFOLDER";
            it.file_type = "FOLDE";
            located(&mut it, store, *id, path)?;
        }
        EventBody::FolderMoved { id, path, old_path } => {
            it.file_type = "FOLDE";
            it.message_type = moved(
                &mut it,
                store,
                *id,
                path,
                old_path,
                "MODIFYFOLDER",
                "MOVEFOLDER",
            )?;
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

/// The fields of a path change: the old name and parent, the new name and parent. A change in
/// the same folder is a rename (`rename`, 4.5 `MODIFY*`), else a move (`MOVE*`, which also
/// carries the target folder's id in `goDirectoryId`). Returns the message type.
fn moved(
    it: &mut Item,
    store: &Store,
    id: u64,
    path: &str,
    old_path: &str,
    rename: &'static str,
    mv: &'static str,
) -> crate::Result<&'static str> {
    located(it, store, id, old_path)?;
    let (go, new_path) = parent_of(store, path)?;
    it.new_name = Some(name_of(path));
    let kind = if Some(&new_path) == it.original_path.as_ref() {
        rename
    } else {
        it.go_directory_id = go;
        mv
    };
    it.new_path = Some(new_path);
    Ok(kind)
}

/// The items of `events` built at `built_ms`: one per file or folder event, in journal order.
/// Fails only when the id registry cannot be read.
pub fn detail_items(
    events: &[Event],
    store: &Store,
    built_ms: u64,
) -> crate::Result<Vec<PushItem>> {
    let mut items = Vec::with_capacity(events.len());
    for event in events {
        if let Some(it) = item(event, store, built_ms)? {
            items.push(PushItem::new(&it, false));
        }
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{FileKind, Origin};
    use crate::push::file_syn_message;
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
        // Not the item time: an item carries the time it is built.
        e.ts = 1;
        e
    }

    /// The items of `events` as JSON text, comma-separated.
    fn items(store: &Store, events: &[Event]) -> String {
        let items = detail_items(events, store, 1000).unwrap();
        let v: Vec<&str> = items.iter().map(|i| i.json()).collect();
        v.join(",")
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

    /// Every item type of section 4.5 that notedown sends, one event each.
    fn every_event() -> Vec<Event> {
        vec![
            ev(Some("SN1"), created()),
            ev(
                None,
                EventBody::FileModified {
                    id: 4711,
                    path: "NOTE/Note/b.note".into(),
                    kind: FileKind::Note,
                    md5: "m6".into(),
                    prev_md5: "m5".into(),
                    size: 20,
                    pages_changed: None,
                },
            ),
            ev(
                None,
                EventBody::FileMoved {
                    id: 4711,
                    path: "NOTE/Note/Work/c.note".into(),
                    old_path: "NOTE/Note/b.note".into(),
                },
            ),
            ev(
                None,
                EventBody::FileMoved {
                    id: 4711,
                    path: "NOTE/Note/c.note".into(),
                    old_path: "NOTE/Note/b.note".into(),
                },
            ),
            ev(
                None,
                EventBody::FileCopied {
                    id: 4712,
                    source_id: 4711,
                    path: "NOTE/Note/Work/d.note".into(),
                },
            ),
            ev(
                Some("SN2"),
                EventBody::FileDeleted {
                    id: 4711,
                    path: "NOTE/Note/e.note".into(),
                    recycle: "r/e.note".into(),
                },
            ),
            ev(
                None,
                EventBody::FolderCreated {
                    id: 77,
                    path: "NOTE/Note/New".into(),
                },
            ),
            ev(
                None,
                EventBody::FolderMoved {
                    id: 77,
                    path: "NOTE/Note/Work/New".into(),
                    old_path: "NOTE/Note/New".into(),
                },
            ),
            ev(
                None,
                EventBody::FolderMoved {
                    id: 77,
                    path: "NOTE/Note/Work/Newer".into(),
                    old_path: "NOTE/Note/Work/New".into(),
                },
            ),
            ev(
                None,
                EventBody::FolderDeleted {
                    id: 77,
                    path: "NOTE/Note/Work/Newer".into(),
                },
            ),
        ]
    }

    /// One golden line per event of [`every_event`].
    const GOLDEN: [&str; 10] = [
        // a new file
        r#"{"messageType":"DOWNLOADFILE","equipmentNo":"null-null","fileType":"FILE","id":4711,"originalName":"a.pdf","newName":"a.pdf","md5":"m5","size":12,"directoryId":1002,"originalPath":"NOTE/Note/Work/","newPath":"NOTE/Note/Work/","timestamp":1000}"#,
        // a changed file is downloaded again
        r#"{"messageType":"DOWNLOADFILE","equipmentNo":"null-null","fileType":"FILE","id":4711,"originalName":"b.note","newName":"b.note","md5":"m6","size":20,"directoryId":1001,"originalPath":"NOTE/Note/","newPath":"NOTE/Note/","timestamp":1000}"#,
        // a move into another folder
        r#"{"messageType":"MOVEFILE","equipmentNo":"null-null","fileType":"FILE","id":4711,"originalName":"b.note","newName":"c.note","directoryId":1001,"goDirectoryId":1002,"originalPath":"NOTE/Note/","newPath":"NOTE/Note/Work/","timestamp":1000}"#,
        // a rename in the same folder
        r#"{"messageType":"MODIFYFILE","equipmentNo":"null-null","fileType":"FILE","id":4711,"originalName":"b.note","newName":"c.note","directoryId":1001,"originalPath":"NOTE/Note/","newPath":"NOTE/Note/","timestamp":1000}"#,
        r#"{"messageType":"COPYFILE","equipmentNo":"null-null","fileType":"FILE","id":4711,"newId":4712,"newName":"d.note","goDirectoryId":1002,"newPath":"NOTE/Note/Work/","timestamp":1000}"#,
        r#"{"messageType":"DELETEFILE","equipmentNo":"null-null","fileType":"FILE","id":4711,"originalName":"e.note","directoryId":1001,"originalPath":"NOTE/Note/","timestamp":1000}"#,
        r#"{"messageType":"ADDFOLDER","equipmentNo":"null-null","fileType":"FOLDE","id":77,"originalName":"New","size":0,"directoryId":1001,"originalPath":"NOTE/Note/","timestamp":1000}"#,
        r#"{"messageType":"MOVEFOLDER","equipmentNo":"null-null","fileType":"FOLDE","id":77,"originalName":"New","newName":"New","directoryId":1001,"goDirectoryId":1002,"originalPath":"NOTE/Note/","newPath":"NOTE/Note/Work/","timestamp":1000}"#,
        r#"{"messageType":"MODIFYFOLDER","equipmentNo":"null-null","fileType":"FOLDE","id":77,"originalName":"New","newName":"Newer","directoryId":1002,"originalPath":"NOTE/Note/Work/","newPath":"NOTE/Note/Work/","timestamp":1000}"#,
        r#"{"messageType":"DELETEFOLDER","equipmentNo":"null-null","fileType":"FOLDE","id":77,"originalName":"Newer","directoryId":1002,"originalPath":"NOTE/Note/Work/","timestamp":1000}"#,
    ];

    #[test]
    fn every_item_type_has_its_golden_json() {
        let (_d, s) = store();
        for (event, want) in every_event().iter().zip(GOLDEN) {
            assert_eq!(items(&s, std::slice::from_ref(event)), want);
        }
    }

    #[test]
    fn no_item_and_no_message_has_null_or_msg() {
        let (_d, s) = store();
        let all = detail_items(&every_event(), &s, 1000).unwrap();
        assert_eq!(all.len(), GOLDEN.len());
        let m = file_syn_message(&all, 5);
        assert!(!m.contains(":null") && !m.contains(r#""msg""#), "{m}");
        let v: serde_json::Value = serde_json::from_str(&m).unwrap();
        fn no_null(v: &serde_json::Value) -> bool {
            match v {
                serde_json::Value::Null => false,
                serde_json::Value::Array(a) => a.iter().all(no_null),
                serde_json::Value::Object(o) => o.values().all(no_null),
                _ => true,
            }
        }
        assert!(no_null(&v), "{m}");
        assert_eq!(v["data"].as_array().unwrap().len(), GOLDEN.len());
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
        let m: serde_json::Value = serde_json::from_str(&file_syn_message(
            &detail_items(&events, &s, 1000).unwrap(),
            5,
        ))
        .unwrap();
        let names: Vec<&str> = m["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["originalName"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["a.note", "b.note", "c.note"]);
    }

    #[test]
    fn an_unknown_or_root_parent_leaves_directory_id_out() {
        let (_d, s) = store();
        let body = EventBody::FileCreated {
            id: 1,
            path: "NOTE/Gone/a.note".into(),
            kind: FileKind::Note,
            md5: "m".into(),
            size: 1,
            pages_changed: None,
        };
        let out = items(&s, &[ev(None, body)]);
        assert!(!out.contains("directoryId"), "{out}");
        assert!(out.contains(r#""originalPath":"NOTE/Gone/""#), "{out}");
        let top = EventBody::FolderCreated {
            id: 5,
            path: "NOTE".into(),
        };
        let out = items(&s, &[ev(None, top)]);
        assert!(!out.contains("directoryId"), "{out}");
        assert!(out.contains(r#""originalPath":"/""#), "{out}");
    }

    #[test]
    fn device_events_give_no_item() {
        let (_d, s) = store();
        assert!(
            detail_items(&[ev(Some("SN1"), EventBody::DeviceBound)], &s, 1000)
                .unwrap()
                .is_empty()
        );
    }
}
