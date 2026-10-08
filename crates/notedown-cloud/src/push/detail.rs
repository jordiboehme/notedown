//! The detail push (`push = "detail"`, the default, D2): one `FILE-SYN` item per changed file
//! or folder, with the fields of section 4.5 of the protocol notes. The pending events are
//! collapsed per entry first ([`Changes`]).
//!
//! Ids come from the event; the parent folder ids are looked up in the id registry of the store
//! (no store lock needed). A field the event cannot fill, or a parent that is not known any
//! more, is left out: Ratta's items never carry `null`.

use serde::Serialize;

use super::PushItem;
use super::changes::{Change, Changes};
use crate::store::{Kind, Store, WirePath};

/// One item. The field order is the order of the 4.5 example; `None` is left out.
#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct Item {
    message_type: &'static str,
    equipment_no: String,
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

/// A fresh item of `change` built at `built_ms`: `equipmentNo` is [`super::equipment_no`] of
/// the device behind its newest event, `timestamp` the build time (4.5).
fn base(change: &Change, built_ms: u64) -> Item {
    Item {
        equipment_no: super::equipment_no(change.device.as_deref()),
        file_type: if change.folder { "FOLDE" } else { "FILE" },
        timestamp: built_ms,
        ..Item::default()
    }
}

/// The items of one collapsed change, by the item rules of 4.5. `known` is the path the device
/// knows (after the folder items of the same push, see [`detail_items`]), `path` the path now.
///
/// - a new entry (created or copied after the last push) gives `DOWNLOADFILE`, `COPYFILE` or
///   `ADDFOLDER` at its path now, or nothing when it is deleted again;
/// - a deleted entry the device knows gives `DELETEFILE` or `DELETEFOLDER` at the path the
///   device knows;
/// - an entry the device knows at another path gives `MODIFY*` (same folder) or `MOVE*`;
/// - a file the device knows with new content gives `DOWNLOADFILE` (after the move, when it
///   moved too: one item alone would leave the device with a stale copy).
fn items_of(
    change: &Change,
    known: Option<&str>,
    path: Option<&str>,
    store: &Store,
    built_ms: u64,
) -> crate::Result<Vec<Item>> {
    let id = change.id;
    let mut out = Vec::new();
    match (known, path) {
        (None, None) => {}
        (None, Some(path)) => {
            let mut it = base(change, built_ms);
            if change.folder {
                it.message_type = "ADDFOLDER";
                located(&mut it, store, id, path)?;
                it.size = Some(0);
            } else if let Some((md5, size)) = &change.content {
                download(&mut it, store, id, path, md5, *size)?;
            } else if let Some(source_id) = change.copy_of {
                it.message_type = "COPYFILE";
                let (go, new_path) = parent_of(store, path)?;
                it.id = Some(source_id);
                it.new_id = Some(id);
                it.new_name = Some(name_of(path));
                it.go_directory_id = go;
                it.new_path = Some(new_path);
            } else {
                return Ok(out);
            }
            out.push(it);
        }
        (Some(known), None) => {
            let mut it = base(change, built_ms);
            it.message_type = if change.folder {
                "DELETEFOLDER"
            } else {
                "DELETEFILE"
            };
            located(&mut it, store, id, known)?;
            out.push(it);
        }
        (Some(known), Some(path)) => {
            if known != path {
                let mut it = base(change, built_ms);
                let (rename, mv) = if change.folder {
                    ("MODIFYFOLDER", "MOVEFOLDER")
                } else {
                    ("MODIFYFILE", "MOVEFILE")
                };
                it.message_type = moved(&mut it, store, id, path, known, rename, mv)?;
                out.push(it);
            }
            if let (false, Some((md5, size))) = (change.folder, &change.content) {
                let mut it = base(change, built_ms);
                download(&mut it, store, id, path, md5, *size)?;
                out.push(it);
            }
        }
    }
    Ok(out)
}

/// A path in the store's display form (`NOTE/Note/a.note`), as far as it parses.
fn norm(path: &str) -> String {
    WirePath::parse_named(path).map_or_else(|_| path.to_string(), |p| p.display())
}

/// The path of a live entry now: the store's path of its id (a parent folder may have moved
/// after the entry's own events), else the path of its newest event. `None` for a deleted
/// entry.
fn final_path(change: &Change, store: &Store) -> crate::Result<Option<String>> {
    let Some(path) = &change.path else {
        return Ok(None);
    };
    let want = if change.folder {
        Kind::Folder
    } else {
        Kind::File
    };
    Ok(Some(match store.kind_and_path_of(change.id)? {
        Some((kind, p)) if kind == want => p.display(),
        _ => norm(path),
    }))
}

/// `path` with its deepest moved ancestor replaced: `moves` are the folders the device knows
/// that the same push moves, as (path the device knows, path now).
fn rebase(path: &str, moves: &[(String, String)]) -> String {
    let mut best: Option<&(String, String)> = None;
    for m in moves {
        let under = path.len() > m.0.len()
            && path.starts_with(m.0.as_str())
            && path.as_bytes()[m.0.len()] == b'/';
        if under && best.is_none_or(|b| m.0.len() > b.0.len()) {
            best = Some(m);
        }
    }
    match best {
        Some((from, to)) => format!("{to}{}", &path[from.len()..]),
        None => path.to_string(),
    }
}

/// The items of `changes` built at `built_ms`: one per changed file or folder (two for a file
/// the device knows that moved and got new content), none for an entry created and deleted
/// again. Fails only when the id registry cannot be read.
///
/// Every live entry is named at its path now ([`final_path`]), and every path the device knows
/// is rebased onto the moves of its parent folders in the same push. The items go out in an
/// order the device can follow: first the folders that are new or moved, parents before
/// children; then the copies (their source is still as the device has it); then the other file
/// items; then the deletes, in journal order. A change of the same entry is in the order of
/// its first event ([`Changes::in_order`]).
pub fn detail_items(
    changes: &Changes,
    store: &Store,
    built_ms: u64,
) -> crate::Result<Vec<PushItem>> {
    let list = changes.in_order();
    let mut paths = Vec::with_capacity(list.len());
    for change in &list {
        paths.push(final_path(change, store)?);
    }
    let moves: Vec<(String, String)> = list
        .iter()
        .zip(&paths)
        .filter(|(c, _)| c.folder)
        .filter_map(|(c, now)| Some((norm(c.known_path.as_ref()?), now.clone()?)))
        .filter(|(from, to)| from != to)
        .collect();
    // (group, depth) per item list: 0 folders, 1 copies, 2 files, 3 deletes.
    let mut groups: Vec<((u8, usize), Vec<Item>)> = Vec::with_capacity(list.len());
    for (change, now) in list.iter().zip(&paths) {
        let known = change
            .known_path
            .as_deref()
            .map(|k| rebase(&norm(k), &moves));
        let items = items_of(change, known.as_deref(), now.as_deref(), store, built_ms)?;
        let key = match now {
            None => (3, 0),
            Some(p) if change.folder => (0, p.matches('/').count()),
            Some(_) if change.known_path.is_none() && change.content.is_none() => (1, 0),
            Some(_) => (2, 0),
        };
        groups.push((key, items));
    }
    // A stable sort: the journal order stays within a group.
    groups.sort_by_key(|(key, _)| *key);
    Ok(groups
        .into_iter()
        .flat_map(|(_, items)| items)
        .map(|it| PushItem::new(&it, false))
        .collect())
}

/// A `DOWNLOADFILE` item: a new and a changed file are both downloaded (4.7).
fn download(
    it: &mut Item,
    store: &Store,
    id: u64,
    path: &str,
    md5: &str,
    size: u64,
) -> crate::Result<()> {
    it.message_type = "DOWNLOADFILE";
    located(it, store, id, path)?;
    it.new_name = it.original_name.clone();
    it.new_path = it.original_path.clone();
    it.md5 = Some(md5.to_string());
    it.size = Some(size);
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{Event, EventBody, FileKind, Origin};
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

    /// The items of `events` (journal order, `seq` from 1) collapsed and built at 1000.
    fn build(store: &Store, events: &[Event]) -> Vec<PushItem> {
        let mut changes = Changes::default();
        for (i, e) in events.iter().enumerate() {
            let mut e = e.clone();
            e.seq = i as u64 + 1;
            changes.apply(&e);
        }
        detail_items(&changes, store, 1000).unwrap()
    }

    /// The items of `events` as JSON text, comma-separated.
    fn items(store: &Store, events: &[Event]) -> String {
        let items = build(store, events);
        let v: Vec<&str> = items.iter().map(|i| i.json()).collect();
        v.join(",")
    }

    /// The `messageType` of each item of `events`.
    fn types(store: &Store, events: &[Event]) -> Vec<String> {
        build(store, events)
            .iter()
            .map(|i| {
                let v: serde_json::Value = serde_json::from_str(i.json()).unwrap();
                v["messageType"].as_str().unwrap().to_string()
            })
            .collect()
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
        r#"{"messageType":"DOWNLOADFILE","equipmentNo":"SN1-null","fileType":"FILE","id":4711,"originalName":"a.pdf","newName":"a.pdf","md5":"m5","size":12,"directoryId":1002,"originalPath":"NOTE/Note/Work/","newPath":"NOTE/Note/Work/","timestamp":1000}"#,
        // a changed file is downloaded again
        r#"{"messageType":"DOWNLOADFILE","equipmentNo":"null-null","fileType":"FILE","id":4711,"originalName":"b.note","newName":"b.note","md5":"m6","size":20,"directoryId":1001,"originalPath":"NOTE/Note/","newPath":"NOTE/Note/","timestamp":1000}"#,
        // a move into another folder
        r#"{"messageType":"MOVEFILE","equipmentNo":"null-null","fileType":"FILE","id":4711,"originalName":"b.note","newName":"c.note","directoryId":1001,"goDirectoryId":1002,"originalPath":"NOTE/Note/","newPath":"NOTE/Note/Work/","timestamp":1000}"#,
        // a rename in the same folder
        r#"{"messageType":"MODIFYFILE","equipmentNo":"null-null","fileType":"FILE","id":4711,"originalName":"b.note","newName":"c.note","directoryId":1001,"originalPath":"NOTE/Note/","newPath":"NOTE/Note/","timestamp":1000}"#,
        r#"{"messageType":"COPYFILE","equipmentNo":"null-null","fileType":"FILE","id":4711,"newId":4712,"newName":"d.note","goDirectoryId":1002,"newPath":"NOTE/Note/Work/","timestamp":1000}"#,
        r#"{"messageType":"DELETEFILE","equipmentNo":"SN2-null","fileType":"FILE","id":4711,"originalName":"e.note","directoryId":1001,"originalPath":"NOTE/Note/","timestamp":1000}"#,
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
        let all: Vec<PushItem> = every_event()
            .iter()
            .flat_map(|e| build(&s, std::slice::from_ref(e)))
            .collect();
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
    fn a_burst_gives_one_item_per_file_in_order() {
        let (_d, s) = store();
        let events: Vec<Event> = ["a", "b", "c"]
            .iter()
            .zip(1..)
            .map(|(n, id)| {
                ev(
                    None,
                    EventBody::FileCreated {
                        id,
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
            serde_json::from_str(&file_syn_message(&build(&s, &events), 5)).unwrap();
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
        assert!(build(&s, &[ev(Some("SN1"), EventBody::DeviceBound)]).is_empty());
    }

    fn modified(path: &str, md5: &str) -> EventBody {
        EventBody::FileModified {
            id: 4711,
            path: path.into(),
            kind: FileKind::Note,
            md5: md5.into(),
            prev_md5: "p".into(),
            size: 20,
            pages_changed: None,
        }
    }

    fn file_moved(old: &str, new: &str) -> EventBody {
        EventBody::FileMoved {
            id: 4711,
            path: new.into(),
            old_path: old.into(),
        }
    }

    fn file_deleted(path: &str) -> EventBody {
        EventBody::FileDeleted {
            id: 4711,
            path: path.into(),
            recycle: "r".into(),
        }
    }

    #[test]
    fn a_new_file_changed_again_is_one_download_with_the_latest_content() {
        let (_d, s) = store();
        let events = [
            ev(None, created()),
            ev(None, modified("NOTE/Note/Work/a.pdf", "m9")),
            ev(None, file_moved("NOTE/Note/Work/a.pdf", "NOTE/Note/b.pdf")),
        ];
        let out = items(&s, &events);
        assert_eq!(
            out,
            r#"{"messageType":"DOWNLOADFILE","equipmentNo":"null-null","fileType":"FILE","id":4711,"originalName":"b.pdf","newName":"b.pdf","md5":"m9","size":20,"directoryId":1001,"originalPath":"NOTE/Note/","newPath":"NOTE/Note/","timestamp":1000}"#
        );
    }

    #[test]
    fn a_new_file_deleted_again_gives_no_item() {
        let (_d, s) = store();
        let events = [
            ev(None, created()),
            ev(None, file_deleted("NOTE/Note/Work/a.pdf")),
        ];
        assert!(build(&s, &events).is_empty());
    }

    #[test]
    fn two_moves_are_one_move_from_the_first_place_to_the_last() {
        let (_d, s) = store();
        let events = [
            ev(None, file_moved("NOTE/Note/a.note", "NOTE/Note/b.note")),
            ev(
                None,
                file_moved("NOTE/Note/b.note", "NOTE/Note/Work/c.note"),
            ),
        ];
        assert_eq!(
            items(&s, &events),
            r#"{"messageType":"MOVEFILE","equipmentNo":"null-null","fileType":"FILE","id":4711,"originalName":"a.note","newName":"c.note","directoryId":1001,"goDirectoryId":1002,"originalPath":"NOTE/Note/","newPath":"NOTE/Note/Work/","timestamp":1000}"#
        );
        // Moved back to where it was: nothing changed for the device.
        let back = [
            ev(None, file_moved("NOTE/Note/a.note", "NOTE/Note/b.note")),
            ev(None, file_moved("NOTE/Note/b.note", "NOTE/Note/a.note")),
        ];
        assert!(build(&s, &back).is_empty());
    }

    #[test]
    fn a_delete_after_a_move_names_the_place_the_device_knows() {
        let (_d, s) = store();
        let events = [
            ev(
                None,
                file_moved("NOTE/Note/a.note", "NOTE/Note/Work/b.note"),
            ),
            ev(Some("SN2"), file_deleted("NOTE/Note/Work/b.note")),
        ];
        assert_eq!(
            items(&s, &events),
            r#"{"messageType":"DELETEFILE","equipmentNo":"SN2-null","fileType":"FILE","id":4711,"originalName":"a.note","directoryId":1001,"originalPath":"NOTE/Note/","timestamp":1000}"#
        );
    }

    #[test]
    fn a_known_file_that_moved_and_changed_gives_the_move_then_the_download() {
        let (_d, s) = store();
        let events = [
            ev(None, modified("NOTE/Note/a.note", "m1")),
            ev(
                None,
                file_moved("NOTE/Note/a.note", "NOTE/Note/Work/a.note"),
            ),
        ];
        assert_eq!(types(&s, &events), ["MOVEFILE", "DOWNLOADFILE"]);
        let out = items(&s, &events);
        assert!(out.ends_with(r#""md5":"m1","size":20,"directoryId":1002,"originalPath":"NOTE/Note/Work/","newPath":"NOTE/Note/Work/","timestamp":1000}"#), "{out}");
    }

    /// Give `path` an id in the store, as the store has it after the events of a test.
    fn assign(s: &Store, path: &str, kind: Kind) -> u64 {
        let g = s.lock().unwrap();
        s.assign(&g, &WirePath::parse_named(path).unwrap(), kind)
            .unwrap()
    }

    /// The items of `events` as parsed JSON.
    fn parsed(s: &Store, events: &[Event]) -> Vec<serde_json::Value> {
        build(s, events)
            .iter()
            .map(|i| serde_json::from_str(i.json()).unwrap())
            .collect()
    }

    fn folder_moved(id: u64, old: &str, new: &str) -> EventBody {
        EventBody::FolderMoved {
            id,
            path: new.into(),
            old_path: old.into(),
        }
    }

    #[test]
    fn a_file_put_into_a_new_folder_that_then_moves_is_named_at_its_place_now() {
        let (_d, s) = store();
        let folder = assign(&s, "NOTE/Note/Work/New", Kind::Folder);
        let file = assign(&s, "NOTE/Note/Work/New/a.note", Kind::File);
        let events = [
            ev(
                None,
                EventBody::FolderCreated {
                    id: folder,
                    path: "NOTE/Note/New".into(),
                },
            ),
            ev(
                None,
                EventBody::FileCreated {
                    id: file,
                    path: "NOTE/Note/New/a.note".into(),
                    kind: FileKind::Note,
                    md5: "m".into(),
                    size: 1,
                    pages_changed: None,
                },
            ),
            ev(
                None,
                folder_moved(folder, "NOTE/Note/New", "NOTE/Note/Work/New"),
            ),
        ];
        let items = parsed(&s, &events);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["messageType"], "ADDFOLDER");
        assert_eq!(items[0]["originalPath"], "NOTE/Note/Work/");
        assert_eq!(items[0]["originalName"], "New");
        assert_eq!(items[1]["messageType"], "DOWNLOADFILE");
        assert_eq!(items[1]["newPath"], "NOTE/Note/Work/New/");
        assert_eq!(items[1]["directoryId"], folder);
    }

    #[test]
    fn a_known_file_changed_then_its_folder_moved_is_downloaded_after_the_move() {
        let (_d, s) = store();
        let folder = assign(&s, "NOTE/Note/Q", Kind::Folder);
        let file = assign(&s, "NOTE/Note/Q/f.note", Kind::File);
        let events = [
            ev(
                None,
                EventBody::FileModified {
                    id: file,
                    path: "NOTE/Note/P/f.note".into(),
                    kind: FileKind::Note,
                    md5: "m2".into(),
                    prev_md5: "m1".into(),
                    size: 3,
                    pages_changed: None,
                },
            ),
            ev(None, folder_moved(folder, "NOTE/Note/P", "NOTE/Note/Q")),
        ];
        let items = parsed(&s, &events);
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0]["messageType"], "MODIFYFOLDER");
        assert_eq!(items[0]["originalName"], "P");
        assert_eq!(items[0]["newName"], "Q");
        assert_eq!(items[1]["messageType"], "DOWNLOADFILE");
        assert_eq!(items[1]["originalPath"], "NOTE/Note/Q/");
        assert_eq!(items[1]["directoryId"], folder);
        assert_eq!(items[1]["md5"], "m2");
    }

    #[test]
    fn nested_folder_moves_go_parent_first_with_rebased_paths() {
        let (_d, s) = store();
        let outer = assign(&s, "NOTE/Note/Work/A2", Kind::Folder);
        let inner = assign(&s, "NOTE/Note/Work/A2/C", Kind::Folder);
        let events = [
            // The inner folder is renamed first, then the outer one moves.
            ev(None, folder_moved(inner, "NOTE/Note/A/B", "NOTE/Note/A/C")),
            ev(
                None,
                folder_moved(outer, "NOTE/Note/A", "NOTE/Note/Work/A2"),
            ),
        ];
        let items = parsed(&s, &events);
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0]["messageType"], "MOVEFOLDER");
        assert_eq!(items[0]["originalPath"], "NOTE/Note/");
        assert_eq!(items[0]["originalName"], "A");
        assert_eq!(items[0]["newPath"], "NOTE/Note/Work/");
        assert_eq!(items[0]["newName"], "A2");
        assert_eq!(items[1]["messageType"], "MODIFYFOLDER");
        assert_eq!(items[1]["originalPath"], "NOTE/Note/Work/A2/");
        assert_eq!(items[1]["originalName"], "B");
        assert_eq!(items[1]["newName"], "C");
        assert_eq!(items[1]["directoryId"], outer);
    }

    #[test]
    fn a_delete_in_a_folder_that_moves_names_the_place_after_the_move() {
        let (_d, s) = store();
        let folder = assign(&s, "NOTE/Note/Q", Kind::Folder);
        let events = [
            ev(None, file_deleted("NOTE/Note/P/e.note")),
            ev(None, folder_moved(folder, "NOTE/Note/P", "NOTE/Note/Q")),
        ];
        let items = parsed(&s, &events);
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0]["messageType"], "MODIFYFOLDER");
        assert_eq!(items[1]["messageType"], "DELETEFILE");
        assert_eq!(items[1]["originalPath"], "NOTE/Note/Q/");
        assert_eq!(items[1]["directoryId"], folder);
    }

    #[test]
    fn a_copy_of_a_new_file_is_a_download_and_a_copy_comes_before_deletes() {
        let (_d, s) = store();
        let copy = |id, source_id, name: &str| EventBody::FileCopied {
            id,
            source_id,
            path: format!("NOTE/Note/{name}"),
        };
        let events = [ev(None, created()), ev(None, copy(4712, 4711, "d.pdf"))];
        let items = parsed(&s, &events);
        let kinds: Vec<&str> = items
            .iter()
            .map(|i| i["messageType"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["DOWNLOADFILE", "DOWNLOADFILE"]);
        assert_eq!(items[1]["id"], 4712);
        assert_eq!(items[1]["md5"], "m5");
        // A source the device has, deleted after the copy: the copy goes first.
        let events = [
            ev(None, copy(4712, 4711, "d.note")),
            ev(None, file_deleted("NOTE/Note/e.note")),
        ];
        assert_eq!(types(&s, &events), ["COPYFILE", "DELETEFILE"]);
    }
}
