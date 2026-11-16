//! The recycle bin of the Mac app: `GET /recycle` lists what the user deleted, on a real socket
//! next to the device routes.

use std::io::Write;

use md5::{Digest, Md5};
use serde_json::{Value, json};

use crate::rest_server::{
    Server, delete, mkdir, q, start, start_imported, upload, uploaded, writer,
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
