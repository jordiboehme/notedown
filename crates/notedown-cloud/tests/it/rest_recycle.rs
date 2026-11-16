//! The recycle bin of the Mac app: `GET /recycle` lists what the user deleted and `POST
//! /recycle/restore` brings an item back, on a real socket next to the device routes.

use std::io::Write;

use md5::{Digest, Md5};
use serde_json::{Value, json};

use notedown_cloud::auth::Accounts;
use notedown_cloud::journal::Origin;
use notedown_cloud::store::retention::Retention;
use time::UtcOffset;

use crate::rest_server::{
    EMAIL, PASSWORD, Server, delete, mkdir, q, send_json, start, start_imported, upload, uploaded,
    writer,
};

fn md5_hex(bytes: &[u8]) -> String {
    const_hex::encode(Md5::digest(bytes))
}

/// The items of `GET /recycle` with `query` (200 expected).
fn bin(s: &Server, token: &str, query: &str) -> (Vec<Value>, bool) {
    let (status, answer) = s.get(Some(token), &format!("/recycle{query}"));
    assert_eq!(status, 200, "{answer}");
    let items = answer["items"].as_array().unwrap().clone();
    (items, answer["more"].as_bool().unwrap())
}

fn keys(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .map(|i| i["key"].as_str().unwrap().to_string())
        .collect()
}

fn index_path(s: &Server) -> std::path::PathBuf {
    s.data.join("accounts/jordi/recycle/index.jsonl")
}

#[test]
fn the_bin_lists_a_deleted_file() {
    let s = start_imported();
    let t = writer(&s);
    let id = uploaded(&s, &t, "NOTE/Note/a.txt", b"bytes");
    assert_eq!(delete(&s, &t, id).0, 200);
    // A read token lists the bin too.
    let (items, more) = bin(&s, &s.token(), "");
    assert!(!more);
    assert_eq!(items.len(), 1, "{items:?}");
    let at = items[0]["deletedAt"].as_u64().unwrap();
    assert_eq!(
        items[0],
        json!({
            "key": format!("{at}/NOTE/Note/a.txt"),
            "path": "NOTE/Note/a.txt",
            "name": "a.txt",
            "kind": "file",
            "deletedAt": at,
            "size": 5,
            "md5": md5_hex(b"bytes"),
        })
    );
}

#[test]
fn the_bin_lists_a_deleted_folder_once() {
    let s = start_imported();
    let t = writer(&s);
    uploaded(&s, &t, "NOTE/Note/W/a.txt", b"aa");
    uploaded(&s, &t, "NOTE/Note/W/S/b.txt", b"bbb");
    let (_, folder) = mkdir(&s, &t, "NOTE/Note/W");
    let w = folder["id"].as_u64().unwrap();
    assert_eq!(delete(&s, &t, w).0, 200);
    let (items, _) = bin(&s, &t, "");
    assert_eq!(items.len(), 1, "{items:?}");
    let at = items[0]["deletedAt"].as_u64().unwrap();
    assert_eq!(
        items[0],
        json!({
            "key": format!("{at}/NOTE/Note/W"),
            "path": "NOTE/Note/W",
            "name": "W",
            "kind": "folder",
            "deletedAt": at,
            "size": 5,
            "files": 2,
        })
    );
}

/// The last visible file of a folder that also holds a dot-file: the delete moves the emptied
/// folder to the bin as well, which the user never deleted.
#[test]
fn a_pruned_parent_is_no_item() {
    let s = start_imported();
    let t = writer(&s);
    let id = uploaded(&s, &t, "NOTE/Note/P/a.txt", b"a");
    std::fs::write(
        s.data.join("accounts/jordi/files/NOTE/Note/P/.DS_Store"),
        b"dots",
    )
    .unwrap();
    let (status, answer) = delete(&s, &t, id);
    assert_eq!(status, 200, "{answer}");
    let index = std::fs::read_to_string(index_path(&s)).unwrap();
    assert!(
        index
            .lines()
            .any(|l| l.contains("\"path\":\"NOTE/Note/P\"") && l.contains("\"pruned\":true")),
        "{index}"
    );
    let (items, _) = bin(&s, &t, "");
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["path"], "NOTE/Note/P/a.txt");
}

#[test]
fn a_replaced_file_is_not_in_the_bin() {
    let s = start_imported();
    let t = writer(&s);
    uploaded(&s, &t, "NOTE/Note/a.txt", b"one");
    let (status, answer) = upload(&s, &t, "NOTE/Note/a.txt", b"two");
    assert_eq!(
        (status, answer["outcome"].as_str()),
        (200, Some("replaced"))
    );
    assert!(
        std::fs::read_to_string(index_path(&s))
            .unwrap()
            .contains("overwritten")
    );
    let (items, more) = bin(&s, &t, "");
    assert_eq!((items, more), (vec![], false));
}

#[test]
fn the_bin_skips_a_line_it_cannot_read() {
    let s = start_imported();
    let t = writer(&s);
    let a = uploaded(&s, &t, "NOTE/Note/a.txt", b"a");
    let b = uploaded(&s, &t, "NOTE/Note/b.txt", b"b");
    assert_eq!(delete(&s, &t, a).0, 200);
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(index_path(&s))
        .unwrap();
    f.write_all(b"this is no line of the index\n").unwrap();
    drop(f);
    // The bad line is in the middle of the index now.
    assert_eq!(delete(&s, &t, b).0, 200);
    let (items, _) = bin(&s, &t, "");
    let mut paths: Vec<&str> = items.iter().map(|i| i["path"].as_str().unwrap()).collect();
    paths.sort_unstable();
    assert_eq!(paths, ["NOTE/Note/a.txt", "NOTE/Note/b.txt"]);
}

#[test]
fn the_bin_answers_scan_hold_and_not_imported() {
    let s = start();
    assert!(s.state.config.protect);
    let (status, answer) = s.get(Some(&s.token()), "/recycle");
    assert_eq!((status, answer), (423, json!({"error": "not_imported"})));

    let s = start_imported();
    let token = s.token();
    let gone = s.put("gone.note", b"g");
    std::fs::remove_file(s.data.join("accounts/jordi/files").join(&gone)).unwrap();
    // The listing's scan sets the hold; the bin does not scan.
    let (status, _) = s.get(Some(&token), &format!("/tree?path={}", q("NOTE/Note")));
    assert_eq!(status, 423);
    let (status, answer) = s.get(Some(&token), "/recycle");
    assert_eq!((status, answer), (423, json!({"error": "scan_hold"})));
}

#[test]
fn the_bin_pages() {
    let s = start_imported();
    let t = writer(&s);
    for name in ["a", "b", "c"] {
        let id = uploaded(&s, &t, &format!("NOTE/Note/{name}.txt"), name.as_bytes());
        assert_eq!(delete(&s, &t, id).0, 200);
    }
    let (all, more) = bin(&s, &t, "");
    assert!(!more);
    assert_eq!(all.len(), 3);
    // Newest first.
    let at: Vec<u64> = all
        .iter()
        .map(|i| i["deletedAt"].as_u64().unwrap())
        .collect();
    assert!(at.windows(2).all(|w| w[0] >= w[1]), "{at:?}");

    let (first, more) = bin(&s, &t, "?limit=1");
    assert_eq!((keys(&first), more), (keys(&all[..1]), true));
    let after = |key: &str| format!("?limit=1&after={}", q(key));
    let (second, more) = bin(&s, &t, &after(&keys(&first)[0]));
    assert_eq!((keys(&second), more), (keys(&all[1..2]), true));
    let (third, more) = bin(&s, &t, &after(&keys(&second)[0]));
    assert_eq!((keys(&third), more), (keys(&all[2..]), false));
    let (rest, more) = bin(&s, &t, &format!("?after={}", q(&keys(&first)[0])));
    assert_eq!((keys(&rest), more), (keys(&all[1..]), false));
    // 0 is 1, as for /events.
    let (one, more) = bin(&s, &t, "?limit=0");
    assert_eq!((one.len(), more), (1, true));

    for bad in ["?limit=x", "?after=nokey", "?after=x%2FNOTE"] {
        let (status, answer) = s.get(Some(&t), &format!("/recycle{bad}"));
        assert_eq!(
            (status, &answer["error"]),
            (400, &json!("bad_request")),
            "{bad}"
        );
    }
}

// ------------------------------------------------------------------------------- restore

/// `POST /recycle/restore` with `body`: (status, answer, `Retry-After`).
fn restore(s: &Server, token: &str, body: Value) -> (u16, Value, Option<String>) {
    send_json(s, token, "POST", "/recycle/restore", Some(body))
}

/// The key of the only item of the bin.
fn only_key(s: &Server, token: &str) -> String {
    let (items, _) = bin(s, token, "");
    assert_eq!(items.len(), 1, "{items:?}");
    items[0]["key"].as_str().unwrap().to_string()
}

/// Every journal event as its JSON line.
fn events(s: &Server) -> Vec<Value> {
    s.state
        .journal
        .read_from(1)
        .unwrap()
        .map(|e| serde_json::from_str(&e.unwrap().to_line()).unwrap())
        .collect()
}

fn files(s: &Server) -> std::path::PathBuf {
    s.data.join("accounts/jordi/files")
}

/// Uploads `bytes` to `path` and deletes it: the key of its item.
fn deleted(s: &Server, token: &str, path: &str, bytes: &[u8]) -> (u64, String) {
    let id = uploaded(s, token, path, bytes);
    assert_eq!(delete(s, token, id).0, 200);
    let (items, _) = bin(s, token, "");
    let item = items.iter().find(|i| i["path"] == path).unwrap();
    (id, item["key"].as_str().unwrap().to_string())
}

#[test]
fn a_restore_answers_the_new_id() {
    let s = start_imported();
    let t = writer(&s);
    let (old, key) = deleted(&s, &t, "NOTE/Note/a.txt", b"bytes");
    let n = events(&s).len();
    let (status, answer, _) = restore(&s, &t, json!({"key": key}));
    assert_eq!(status, 200, "{answer}");
    let id = answer["id"].as_u64().unwrap();
    assert_ne!(id, old);
    let seqs = answer["seqs"].as_array().unwrap().clone();
    assert_eq!(seqs.len(), 1, "{answer}");
    assert_eq!(
        answer,
        json!({"id": id, "path": "NOTE/Note/a.txt", "kind": "file", "seqs": seqs})
    );
    let r = s.request("GET", Some(&t), &format!("/files/{id}"));
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.bytes().unwrap().as_ref(), b"bytes");
    let new = events(&s)[n..].to_vec();
    assert_eq!(new.len(), 1, "{new:?}");
    assert_eq!(new[0]["type"], "file.created");
    assert_eq!(new[0]["origin"], "app");
    assert_eq!(new[0]["id"], id);
    assert_eq!(new[0]["seq"], seqs[0]);
    assert!(
        new[0].get("device").is_none_or(Value::is_null),
        "{}",
        new[0]
    );
    assert_eq!(bin(&s, &t, "").0, Vec::<Value>::new());
}

#[test]
fn a_taken_path_is_name_taken_and_another_path_works() {
    let s = start_imported();
    let t = writer(&s);
    let (_, key) = deleted(&s, &t, "NOTE/Note/a.txt", b"old");
    // Another spelling of the name is taken too.
    uploaded(&s, &t, "NOTE/Note/A.txt", b"new");
    let n = events(&s).len();
    let (status, answer, _) = restore(&s, &t, json!({"key": key}));
    assert_eq!(status, 409, "{answer}");
    assert_eq!(answer, json!({"error": "name_taken", "existing": "A.txt"}));
    assert_eq!(events(&s).len(), n);
    assert_eq!(keys(&bin(&s, &t, "").0), std::slice::from_ref(&key));

    let (status, answer, _) = restore(&s, &t, json!({"key": key, "path": "NOTE/Note/Old/b.txt"}));
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["path"], "NOTE/Note/Old/b.txt");
    assert_eq!(
        std::fs::read(files(&s).join("NOTE/Note/Old/b.txt")).unwrap(),
        b"old"
    );
    // The missing folder came first.
    let new = events(&s)[n..].to_vec();
    let types: Vec<&str> = new.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["folder.created", "file.created"], "{new:?}");
}

#[test]
fn a_folder_restore_into_a_taken_path_is_name_taken() {
    let s = start_imported();
    let t = writer(&s);
    uploaded(&s, &t, "NOTE/Note/W/a.txt", b"aa");
    uploaded(&s, &t, "NOTE/Note/W/S/b.txt", b"bbb");
    let (_, folder) = mkdir(&s, &t, "NOTE/Note/W");
    assert_eq!(delete(&s, &t, folder["id"].as_u64().unwrap()).0, 200);
    let key = only_key(&s, &t);
    // A folder is never merged into a folder that is there.
    assert_eq!(mkdir(&s, &t, "NOTE/Note/W").0, 201);
    let (status, answer, _) = restore(&s, &t, json!({"key": key}));
    assert_eq!(status, 409, "{answer}");
    assert_eq!(answer, json!({"error": "name_taken", "existing": "W"}));

    let (status, answer, _) = restore(&s, &t, json!({"key": key, "path": "NOTE/Note/W2"}));
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["kind"], "folder");
    assert_eq!(answer["path"], "NOTE/Note/W2");
    assert_eq!(
        std::fs::read(files(&s).join("NOTE/Note/W2/S/b.txt")).unwrap(),
        b"bbb"
    );
    assert!(bin(&s, &t, "").0.is_empty());
}

#[test]
fn a_restore_under_a_file_is_not_a_folder() {
    let s = start_imported();
    let t = writer(&s);
    let (_, key) = deleted(&s, &t, "NOTE/Note/a.txt", b"a");
    uploaded(&s, &t, "NOTE/Note/f.txt", b"f");
    let (status, answer, _) = restore(&s, &t, json!({"key": key, "path": "NOTE/Note/f.txt/a.txt"}));
    assert_eq!(status, 409, "{answer}");
    assert_eq!(answer, json!({"error": "not_a_folder"}));
    assert_eq!(keys(&bin(&s, &t, "").0), [key]);
}

#[test]
fn a_restore_outside_the_device_trees_is_not_writable() {
    let s = start_imported();
    let t = writer(&s);
    let (_, key) = deleted(&s, &t, "NOTE/Note/a.txt", b"a");
    for path in ["NOTE/a.txt", "NOTE/Note", "other/a.txt"] {
        let (status, answer, _) = restore(&s, &t, json!({"key": key, "path": path}));
        assert_eq!(
            (status, answer),
            (403, json!({"error": "not_writable"})),
            "{path}"
        );
    }
    let (status, answer, _) = restore(&s, &t, json!({"key": key, "path": "NOTE/Note/.a"}));
    assert_eq!((status, answer), (400, json!({"error": "bad_name"})));
    for body in [
        json!({"path": "NOTE/Note/a.txt"}),
        json!({"key": 5}),
        json!([]),
    ] {
        let (status, answer, _) = restore(&s, &t, body.clone());
        assert_eq!(
            (status, &answer["error"]),
            (400, &json!("bad_request")),
            "{body}"
        );
    }
    assert_eq!(keys(&bin(&s, &t, "").0), [key]);
}

#[test]
fn a_restore_waits_while_a_device_syncs() {
    let s = start_imported();
    let t = writer(&s);
    let (_, key) = deleted(&s, &t, "NOTE/Note/a.txt", b"a");
    s.state
        .sync_locks
        .start("jordi", "SN078C10000000", s.state.now_ms());
    let n = events(&s).len();
    let (status, answer, retry) = restore(&s, &t, json!({"key": key}));
    assert_eq!(status, 423, "{answer}");
    assert_eq!(answer, json!({"error": "device_syncing"}));
    assert_eq!(retry.as_deref(), Some("5"));
    assert_eq!(events(&s).len(), n);
    // Reads go on: the item is still listed.
    assert_eq!(keys(&bin(&s, &t, "").0), [key]);
}

#[test]
fn restore_answers_scan_hold() {
    let s = start();
    assert!(s.state.config.protect);
    let t = writer(&s);
    // Any key: the gate comes before the lookup.
    let (status, answer, _) = restore(&s, &t, json!({"key": "1/NOTE/Note/a.txt"}));
    assert_eq!((status, answer), (423, json!({"error": "not_imported"})));

    let s = start_imported();
    let t = writer(&s);
    let (_, key) = deleted(&s, &t, "NOTE/Note/a.txt", b"a");
    let gone = s.put("gone.note", b"g");
    std::fs::remove_file(files(&s).join(&gone)).unwrap();
    assert_eq!(
        s.get(Some(&t), &format!("/tree?path={}", q("NOTE/Note"))).0,
        423
    );
    let (status, answer, _) = restore(&s, &t, json!({"key": key}));
    assert_eq!((status, answer), (423, json!({"error": "scan_hold"})));
    assert!(!files(&s).join("NOTE/Note/a.txt").exists());
}

#[test]
fn a_restore_needs_a_write_token() {
    let s = start_imported();
    let t = writer(&s);
    let (_, key) = deleted(&s, &t, "NOTE/Note/a.txt", b"a");
    let (status, answer, _) = restore(&s, &s.token(), json!({"key": key}));
    assert_eq!((status, answer), (403, json!({"error": "read_only_token"})));
    let (status, answer, _) = restore(&s, "no-token", json!({"key": key}));
    assert_eq!((status, answer), (401, json!({"error": "token_refused"})));
    assert_eq!(keys(&bin(&s, &t, "").0), [key]);
}

#[test]
fn a_second_restore_of_the_same_item_is_not_found() {
    let s = start_imported();
    let t = writer(&s);
    let (_, key) = deleted(&s, &t, "NOTE/Note/a.txt", b"a");
    assert_eq!(restore(&s, &t, json!({"key": key})).0, 200);
    let n = events(&s).len();
    let (status, answer, _) = restore(&s, &t, json!({"key": key}));
    assert_eq!((status, answer), (404, json!({"error": "not_found"})));
    let (status, answer, _) = restore(&s, &t, json!({"key": "1/NOTE/Note/none.txt"}));
    assert_eq!((status, answer), (404, json!({"error": "not_found"})));
    assert_eq!(events(&s).len(), n);
}

#[test]
fn a_restore_of_an_item_the_prune_removed_is_not_found() {
    const DAY: u64 = 24 * 60 * 60 * 1000;
    let s = start_imported();
    let t = writer(&s);
    let (_, key) = deleted(&s, &t, "NOTE/Note/a.txt", b"a");
    let retention = Retention {
        keep_latest: 3,
        keep_daily: 7,
        keep_monthly: 12,
        keep_deleted_days: 90,
    };
    let report = s
        .state
        .store("jordi")
        .unwrap()
        .prune_recycle(
            &s.state.journal,
            &retention,
            s.state.now_ms() + 100 * DAY,
            UtcOffset::UTC,
            Origin::Cli,
            false,
        )
        .unwrap();
    assert_eq!(report.removed.len(), 1, "{report:?}");
    let (status, answer, _) = restore(&s, &t, json!({"key": key}));
    assert_eq!((status, answer), (404, json!({"error": "not_found"})));
    assert!(!files(&s).join("NOTE/Note/a.txt").exists());
}

#[test]
fn a_restore_counts_against_the_quota() {
    let s = start_imported();
    let t = writer(&s);
    let accounts = Accounts::new(&s.data);
    let mut a = accounts.get("jordi").unwrap().unwrap();
    a.quota_bytes = Some(10);
    accounts.save(&a).unwrap();
    let (_, key) = deleted(&s, &t, "NOTE/Note/a.txt", b"12345678");
    let b = uploaded(&s, &t, "NOTE/Note/b.txt", b"12345");
    let n = events(&s).len();
    let (status, answer, _) = restore(&s, &t, json!({"key": key}));
    assert_eq!((status, answer), (507, json!({"error": "quota_exceeded"})));
    assert_eq!(events(&s).len(), n);
    assert_eq!(keys(&bin(&s, &t, "").0), std::slice::from_ref(&key));

    assert_eq!(delete(&s, &t, b).0, 200);
    let (status, answer, _) = restore(&s, &t, json!({"key": key}));
    assert_eq!(status, 200, "{answer}");
    // The cached usage learned the 8 bytes: 3 more are too many.
    let (status, answer) = upload(&s, &t, "NOTE/Note/c.txt", b"123");
    assert_eq!((status, answer), (507, json!({"error": "quota_exceeded"})));
}

#[cfg(unix)]
#[test]
fn a_restore_of_a_damaged_slot_is_slot_damaged() {
    let s = start_imported();
    let t = writer(&s);
    uploaded(&s, &t, "NOTE/Note/W/a.txt", b"a");
    let (_, folder) = mkdir(&s, &t, "NOTE/Note/W");
    assert_eq!(delete(&s, &t, folder["id"].as_u64().unwrap()).0, 200);
    let key = only_key(&s, &t);
    let slot = s.data.join("accounts/jordi/recycle").join(&key);
    std::os::unix::fs::symlink("/etc", slot.join("link")).unwrap();
    let n = events(&s).len();
    let (status, answer, _) = restore(&s, &t, json!({"key": key}));
    assert_eq!((status, answer), (409, json!({"error": "slot_damaged"})));
    assert_eq!(events(&s).len(), n);
    assert!(!files(&s).join("NOTE/Note/W").exists());
}

#[test]
fn a_restore_after_a_device_delete_shows_the_file_to_the_device() {
    use crate::fake_device::{Device, SERIAL, sign_in};
    let s = start_imported();
    let t = writer(&s);
    let old = uploaded(&s, &t, "NOTE/Note/a.txt", b"bytes");
    let mut d = sign_in(s.base.clone(), EMAIL, PASSWORD);
    let start = json!({"equipmentNo": SERIAL});
    let end = json!({"equipmentNo": SERIAL, "flag": "Y"});
    d.ok("/api/file/2/files/synchronous/start", start.clone());
    let id = Device::id_of(&d.listing(), "NOTE/Note/a.txt");
    assert_eq!(id, old.to_string());
    d.ok(
        "/api/file/3/files/query_v3",
        json!({"equipmentNo": SERIAL, "id": id}),
    );
    d.ok(
        "/api/file/3/files/delete_folder_v3",
        json!({"equipmentNo": SERIAL, "id": id}),
    );
    d.ok("/api/file/2/files/synchronous/end", end.clone());

    let key = only_key(&s, &t);
    let (status, answer, _) = restore(&s, &t, json!({"key": key}));
    assert_eq!(status, 200, "{answer}");
    let new = answer["id"].as_u64().unwrap();
    assert_ne!(new, old);

    d.ok("/api/file/2/files/synchronous/start", start);
    let entries = d.listing();
    assert_eq!(Device::id_of(&entries, "NOTE/Note/a.txt"), new.to_string());
    d.ok("/api/file/2/files/synchronous/end", end);
}

// ------------------------------------------------------------------------------- empty

/// `POST /recycle/empty` without a body: (status, answer, `Retry-After`).
fn empty(s: &Server, token: &str) -> (u16, Value, Option<String>) {
    send_json(s, token, "POST", "/recycle/empty", None)
}

/// The lines of the bin index (no index: none).
fn index_lines(s: &Server) -> Vec<Value> {
    std::fs::read_to_string(index_path(s))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn recycle_dir(s: &Server) -> std::path::PathBuf {
    s.data.join("accounts/jordi/recycle")
}

#[test]
fn empty_removes_deleted_files_and_folders() {
    let s = start_imported();
    let t = writer(&s);
    let (_, file) = deleted(&s, &t, "NOTE/Note/c.txt", b"cccc");
    uploaded(&s, &t, "NOTE/Note/W/a.txt", b"aa");
    uploaded(&s, &t, "NOTE/Note/W/S/b.txt", b"bbb");
    let (_, folder) = mkdir(&s, &t, "NOTE/Note/W");
    assert_eq!(delete(&s, &t, folder["id"].as_u64().unwrap()).0, 200);
    let (items, _) = bin(&s, &t, "");
    assert_eq!(items.len(), 2, "{items:?}");
    let slots: Vec<_> = keys(&items)
        .iter()
        .map(|k| recycle_dir(&s).join(k))
        .collect();
    assert!(slots.iter().all(|p| p.exists()));
    let n = events(&s).len();

    let (status, answer, _) = empty(&s, &t);
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer, json!({"removed": 2, "bytes": 4 + 2 + 3, "kept": 0}));
    assert_eq!(bin(&s, &t, "").0, Vec::<Value>::new());
    assert!(slots.iter().all(|p| !p.exists()), "{file}");
    assert_eq!(index_lines(&s), Vec::<Value>::new());
    // One event per line: the file, the folder's two files and the folder itself.
    let new = events(&s)[n..].to_vec();
    assert_eq!(new.len(), 4, "{new:?}");
    for e in &new {
        assert_eq!(e["type"], "recycle.pruned");
        assert_eq!(e["origin"], "app");
        assert_eq!(e["opId"], new[0]["opId"]);
        assert_eq!(e["reason"], "deleted");
        assert!(e.get("device").is_none_or(Value::is_null), "{e}");
    }
    let mut paths: Vec<&str> = new.iter().map(|e| e["path"].as_str().unwrap()).collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        [
            "NOTE/Note/W",
            "NOTE/Note/W/S/b.txt",
            "NOTE/Note/W/a.txt",
            "NOTE/Note/c.txt"
        ]
    );
}

/// A folder that a delete left with only dot-files is in the bin, but no item: Empty takes it.
#[test]
fn empty_removes_a_pruned_parent_too() {
    let s = start_imported();
    let t = writer(&s);
    let id = uploaded(&s, &t, "NOTE/Note/P/a.txt", b"a");
    std::fs::write(files(&s).join("NOTE/Note/P/.DS_Store"), b"dots").unwrap();
    assert_eq!(delete(&s, &t, id).0, 200);
    assert_eq!(index_lines(&s).len(), 2);
    let (status, answer, _) = empty(&s, &t);
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer, json!({"removed": 1, "bytes": 1, "kept": 0}));
    assert_eq!(index_lines(&s), Vec::<Value>::new());
}

#[test]
fn empty_keeps_old_versions_of_live_files() {
    let s = start_imported();
    let t = writer(&s);
    let live = uploaded(&s, &t, "NOTE/Note/a.txt", b"one");
    assert_eq!(upload(&s, &t, "NOTE/Note/a.txt", b"two").0, 200);
    deleted(&s, &t, "NOTE/Note/b.txt", b"bb");
    let old = index_lines(&s)
        .into_iter()
        .find(|l| l["reason"] == "overwritten")
        .unwrap();
    let (status, answer, _) = empty(&s, &t);
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer, json!({"removed": 1, "bytes": 2, "kept": 0}));
    assert_eq!(index_lines(&s), std::slice::from_ref(&old));
    assert_eq!(old["id"], live);
    let slot = recycle_dir(&s).join(format!(
        "{}/{}",
        old["deletedAt"],
        old["path"].as_str().unwrap()
    ));
    assert_eq!(std::fs::read(slot).unwrap(), b"one");
}

#[test]
fn empty_keeps_the_old_versions_of_a_restored_file() {
    let s = start_imported();
    let t = writer(&s);
    let old = uploaded(&s, &t, "NOTE/Note/a.txt", b"one");
    assert_eq!(upload(&s, &t, "NOTE/Note/a.txt", b"two").0, 200);
    assert_eq!(delete(&s, &t, old).0, 200);
    let key = only_key(&s, &t);
    let (status, answer, _) = restore(&s, &t, json!({"key": key}));
    assert_eq!(status, 200, "{answer}");
    let new = answer["id"].as_u64().unwrap();
    // Something to remove, so Empty writes the index.
    deleted(&s, &t, "NOTE/Note/b.txt", b"bb");

    let (status, answer, _) = empty(&s, &t);
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer, json!({"removed": 1, "bytes": 2, "kept": 0}));
    let lines = index_lines(&s);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["reason"], "overwritten");
    assert_eq!(lines[0]["id"], new);
    let slot = recycle_dir(&s).join(format!(
        "{}/{}",
        lines[0]["deletedAt"],
        lines[0]["path"].as_str().unwrap()
    ));
    assert_eq!(std::fs::read(slot).unwrap(), b"one");
    assert_eq!(
        std::fs::read(files(&s).join("NOTE/Note/a.txt")).unwrap(),
        b"two"
    );
}

#[test]
fn empty_with_nothing_to_remove() {
    let s = start_imported();
    let t = writer(&s);
    let n = events(&s).len();
    let (status, answer, _) = empty(&s, &t);
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer, json!({"removed": 0, "bytes": 0, "kept": 0}));
    // Old versions alone: nothing to remove either.
    uploaded(&s, &t, "NOTE/Note/a.txt", b"one");
    assert_eq!(upload(&s, &t, "NOTE/Note/a.txt", b"two").0, 200);
    let n2 = events(&s).len();
    assert!(n2 > n);
    let (status, answer, _) = empty(&s, &t);
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer, json!({"removed": 0, "bytes": 0, "kept": 0}));
    assert_eq!(events(&s).len(), n2);
    assert_eq!(index_lines(&s).len(), 1);
}

#[test]
fn empty_waits_while_a_device_syncs() {
    let s = start_imported();
    let t = writer(&s);
    let (_, key) = deleted(&s, &t, "NOTE/Note/a.txt", b"a");
    s.state
        .sync_locks
        .start("jordi", "SN078C10000000", s.state.now_ms());
    let n = events(&s).len();
    let (status, answer, retry) = empty(&s, &t);
    assert_eq!(status, 423, "{answer}");
    assert_eq!(answer, json!({"error": "device_syncing"}));
    assert_eq!(retry.as_deref(), Some("5"));
    assert_eq!(events(&s).len(), n);
    assert_eq!(keys(&bin(&s, &t, "").0), [key]);
}

#[test]
fn empty_answers_not_imported_and_scan_hold() {
    let s = start();
    assert!(s.state.config.protect);
    let t = writer(&s);
    let (status, answer, _) = empty(&s, &t);
    assert_eq!((status, answer), (423, json!({"error": "not_imported"})));

    let s = start_imported();
    let t = writer(&s);
    let (_, key) = deleted(&s, &t, "NOTE/Note/a.txt", b"a");
    let gone = s.put("gone.note", b"g");
    std::fs::remove_file(files(&s).join(&gone)).unwrap();
    assert_eq!(
        s.get(Some(&t), &format!("/tree?path={}", q("NOTE/Note"))).0,
        423
    );
    let n = events(&s).len();
    let (status, answer, _) = empty(&s, &t);
    assert_eq!((status, answer), (423, json!({"error": "scan_hold"})));
    assert_eq!(events(&s).len(), n);
    assert!(recycle_dir(&s).join(&key).exists());
}

#[test]
fn empty_needs_a_write_token() {
    let s = start_imported();
    let t = writer(&s);
    let (_, key) = deleted(&s, &t, "NOTE/Note/a.txt", b"a");
    let (status, answer, _) = empty(&s, &s.token());
    assert_eq!((status, answer), (403, json!({"error": "read_only_token"})));
    let (status, answer, _) = empty(&s, "no-token");
    assert_eq!((status, answer), (401, json!({"error": "token_refused"})));
    assert_eq!(keys(&bin(&s, &t, "").0), [key]);
    // Only POST.
    let (status, answer) = s.get(Some(&t), "/recycle/empty");
    assert_eq!((status, answer), (404, json!({"error": "no_route"})));
}

#[test]
fn empty_then_the_hourly_prune_finds_nothing() {
    let s = start_imported();
    let t = writer(&s);
    uploaded(&s, &t, "NOTE/Note/a.txt", b"one");
    assert_eq!(upload(&s, &t, "NOTE/Note/a.txt", b"two").0, 200);
    deleted(&s, &t, "NOTE/Note/b.txt", b"bb");
    assert_eq!(empty(&s, &t).0, 200);
    let n = events(&s).len();
    let lines = index_lines(&s);
    let report = s.state.prune_recycle_all().unwrap();
    assert_eq!(report.removed, 0, "{report:?}");
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    assert_eq!(events(&s).len(), n);
    assert_eq!(index_lines(&s), lines);
}
