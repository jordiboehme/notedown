//! The REST routes of the Mac app: browse (`/tree`, `/entries/{id}`), download
//! (`/files/{id}`), upload (`PUT /files?path=`), new folders (`POST /folders`), move (`POST
//! /move`) and delete (`DELETE /entries/{id}`), on a real socket next to the device routes.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::PathBuf;

use md5::{Digest, Md5};
use notedown_cloud::auth::Accounts;
use notedown_cloud::put;
use notedown_cloud::store::WirePath;
use serde_json::{Value, json};

use crate::rest_server::{
    EMAIL, PASSWORD, Server, delete, header, json_of, mkdir, mv, q, send_json, start,
    start_imported, tree_all, upload, upload_with, uploaded, writer,
};

fn md5_hex(bytes: &[u8]) -> String {
    const_hex::encode(Md5::digest(bytes))
}

fn files(s: &Server) -> PathBuf {
    s.data.join("accounts/jordi/files")
}

/// Writes `bytes` at the wire path `rel` by hand, past the store.
fn hand(s: &Server, rel: &str, bytes: &[u8]) {
    let p = files(s).join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

fn id_of(s: &Server, path: &str) -> u64 {
    s.state
        .store("jordi")
        .unwrap()
        .id_of(&WirePath::parse(path).unwrap())
        .unwrap()
        .unwrap()
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

fn paths(answer: &Value) -> Vec<String> {
    answer["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap().to_string())
        .collect()
}

fn find<'a>(answer: &'a Value, path: &str) -> &'a Value {
    answer["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == path)
        .unwrap_or_else(|| panic!("{path} not in {answer}"))
}

/// A GET of `path` with the token and `headers`.
fn get_with(
    s: &Server,
    token: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> reqwest::blocking::Response {
    let mut req = s.client.get(s.url(path)).bearer_auth(token);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    req.send().unwrap()
}

#[test]
fn tree_lists_the_children_of_the_root() {
    let s = start_imported();
    let token = s.token();
    let (_, entries) = tree_all(&s, &token, "");
    let answer = json!({ "entries": entries });
    let got: BTreeSet<String> = paths(&answer).into_iter().collect();
    let want: BTreeSet<String> = ["DOCUMENT", "EXPORT", "INBOX", "NOTE", "SCREENSHOT"]
        .into_iter()
        .map(str::to_string)
        .collect();
    assert_eq!(got, want, "{answer}");
    for e in answer["entries"].as_array().unwrap() {
        assert_eq!(e["kind"], "folder", "{e}");
        assert_eq!(e["writable"], false, "{e}");
        assert!(e.get("md5").is_none(), "{e}");
        assert!(e["id"].is_u64(), "{e}");
        assert_eq!(e["name"], e["path"], "{e}");
    }
    // The trees below NOTE and DOCUMENT are no more writable than their parents.
    let (status, answer) = s.get(Some(&token), "/tree?path=NOTE");
    assert_eq!(status, 200, "{answer}");
    assert_eq!(paths(&answer), vec!["NOTE/MyStyle", "NOTE/Note"]);
    assert_eq!(find(&answer, "NOTE/Note")["writable"], false);
    let (_, answer) = s.get(Some(&token), "/tree?path=DOCUMENT");
    assert_eq!(paths(&answer), vec!["DOCUMENT/Document"]);
    assert_eq!(find(&answer, "DOCUMENT/Document")["writable"], false);
}

#[test]
fn tree_lists_a_folder_recursively() {
    let s = start_imported();
    let token = s.token();
    let src = s.data.parent().unwrap().join("b.pdf");
    std::fs::write(&src, b"a book").unwrap();
    put::put(
        &s.data,
        "jordi",
        &s.state.journal,
        &src,
        "DOCUMENT/Document/books",
        None,
    )
    .unwrap();
    s.put("a.note", b"note");

    let (status, answer) = s.get(
        Some(&token),
        &format!("/tree?path={}&recursive=true", q("DOCUMENT/Document")),
    );
    assert_eq!(status, 200, "{answer}");
    assert_eq!(
        paths(&answer),
        vec!["DOCUMENT/Document/books", "DOCUMENT/Document/books/b.pdf"]
    );
    let store = s.state.store("jordi").unwrap();
    let g = store.lock().unwrap();
    let listed = store
        .list(&g, &WirePath::parse("DOCUMENT/Document").unwrap(), true)
        .unwrap();
    drop(g);
    for e in &listed {
        let got = find(&answer, &e.path.display());
        assert_eq!(got["id"], json!(e.id));
        assert_eq!(got["name"], json!(e.name));
        assert_eq!(got["size"], json!(e.size));
        assert_eq!(got["mtimeMs"], json!(e.last_update_time));
        assert_eq!(got["writable"], true);
        match &e.content_hash {
            Some(md5) => {
                assert_eq!(got["md5"], json!(md5));
                assert_eq!(got["kind"], "file");
            }
            None => {
                assert!(got.get("md5").is_none(), "{got}");
                assert_eq!(got["kind"], "folder");
            }
        }
    }
    let book = find(&answer, "DOCUMENT/Document/books/b.pdf");
    assert_eq!(book["md5"], md5_hex(b"a book"));
    assert_eq!(book["size"], 6);
    // Without `recursive` only the children.
    let (status, answer) = s.get(
        Some(&token),
        &format!("/tree?path={}", q("DOCUMENT/Document")),
    );
    assert_eq!(status, 200);
    assert_eq!(paths(&answer), vec!["DOCUMENT/Document/books"]);
    let (status, answer) = s.get(Some(&token), "/tree?recursive=maybe");
    assert_eq!((status, &answer["error"]), (400, &json!("bad_request")));
}

#[test]
fn tree_gives_the_journal_seq() {
    let s = start_imported();
    let token = s.token();
    s.put("a.note", b"a");
    let (status, answer) = s.get(Some(&token), "/tree");
    assert_eq!(status, 200);
    let first = answer["seq"].as_u64().unwrap();
    assert!(first > 0);
    assert_eq!(first, s.state.journal.last_seq().unwrap());
    s.put("b.note", b"b");
    let (_, answer) = s.get(Some(&token), &format!("/tree?path={}", q("NOTE/Note")));
    let second = answer["seq"].as_u64().unwrap();
    assert!(second > first);
    assert_eq!(second, s.state.journal.last_seq().unwrap());
}

#[test]
fn tree_of_a_missing_path_is_404_and_of_a_file_409() {
    let s = start_imported();
    let token = s.token();
    let path = s.put("a.note", b"a");
    let (status, answer) = s.get(Some(&token), &format!("/tree?path={}", q("NOTE/Note/nope")));
    assert_eq!((status, answer), (404, json!({"error": "not_found"})));
    let (status, answer) = s.get(Some(&token), &format!("/tree?path={}", q(&path)));
    assert_eq!((status, answer), (409, json!({"error": "not_a_folder"})));
    // Names are exact: another case is no folder.
    let (status, answer) = s.get(Some(&token), &format!("/tree?path={}", q("note/Note")));
    assert_eq!((status, answer), (404, json!({"error": "not_found"})));
    let (status, answer) = s.get(Some(&token), &format!("/tree?path={}", q("NOTE/../NOTE")));
    assert_eq!((status, &answer["error"]), (400, &json!("bad_request")));
}

#[test]
fn tree_journals_a_file_made_by_hand_before_it_lists_it() {
    let s = start_imported();
    let token = s.token();
    hand(&s, "NOTE/Note/hand.note", b"by hand");
    let (status, answer) = s.get(Some(&token), &format!("/tree?path={}", q("NOTE/Note")));
    assert_eq!(status, 200, "{answer}");
    let entry = find(&answer, "NOTE/Note/hand.note");
    let id = entry["id"].as_u64().unwrap();
    let created = events(&s)
        .into_iter()
        .find(|e| e["type"] == "file.created" && e["path"] == "NOTE/Note/hand.note")
        .expect("the scan journals the file");
    assert_eq!(created["origin"], "scan");
    assert_eq!(created["id"], json!(id));
    assert_eq!(created["md5"], md5_hex(b"by hand"));
    assert!(created["seq"].as_u64().unwrap() <= answer["seq"].as_u64().unwrap());
    // A second listing journals nothing new.
    let before = events(&s).len();
    let (status, _) = s.get(Some(&token), &format!("/tree?path={}", q("NOTE/Note")));
    assert_eq!(status, 200);
    assert_eq!(events(&s).len(), before);
}

#[test]
fn browse_waits_while_a_scan_holds_the_account() {
    let s = start_imported();
    let token = s.token();
    let path = s.put("a.note", b"a");
    let id = id_of(&s, &path);
    std::fs::remove_file(files(&s).join(&path)).unwrap();
    let held = json!({"error": "scan_hold"});
    let (status, answer) = s.get(Some(&token), &format!("/tree?path={}", q("NOTE/Note")));
    assert_eq!((status, &answer), (423, &held));
    for p in [
        "/tree".to_string(),
        format!("/entries/{id}"),
        format!("/files/{id}"),
    ] {
        let (status, answer) = s.get(Some(&token), &p);
        assert_eq!((status, &answer), (423, &held), "{p}");
    }
    let r = s.request("HEAD", Some(&token), &format!("/files/{id}"));
    assert_eq!(r.status().as_u16(), 423);
}

#[test]
fn browse_waits_for_the_import_in_protect_mode() {
    let s = start();
    assert!(s.state.config.protect);
    let token = s.token();
    let closed = json!({"error": "not_imported"});
    for p in ["/tree", "/entries/1000", "/files/1000"] {
        let (status, answer) = s.get(Some(&token), p);
        assert_eq!((status, &answer), (423, &closed), "{p}");
    }
}

#[test]
fn an_entry_by_id() {
    let s = start_imported();
    let token = s.token();
    let path = s.put("a.note", b"abc");
    let id = id_of(&s, &path);
    let (status, entry) = s.get(Some(&token), &format!("/entries/{id}"));
    assert_eq!(status, 200, "{entry}");
    let (_, listing) = s.get(Some(&token), &format!("/tree?path={}", q("NOTE/Note")));
    assert_eq!(&entry, find(&listing, &path));
    assert_eq!(entry["id"], json!(id));
    assert_eq!(entry["path"], path);
    assert_eq!(entry["name"], "a.note");
    assert_eq!(entry["kind"], "file");
    assert_eq!(entry["size"], 3);
    assert_eq!(entry["md5"], md5_hex(b"abc"));
    assert_eq!(entry["writable"], true);
    assert!(entry["mtimeMs"].as_u64().unwrap() > 0);
    // A folder too.
    let folder = id_of(&s, "NOTE/Note");
    let (status, entry) = s.get(Some(&token), &format!("/entries/{folder}"));
    assert_eq!(status, 200, "{entry}");
    assert_eq!(entry["kind"], "folder");
    assert_eq!(entry["path"], "NOTE/Note");
    assert_eq!(entry["writable"], false);
    assert!(entry.get("md5").is_none());
    // An id that is no number.
    let (status, answer) = s.get(Some(&token), "/entries/abc");
    assert_eq!((status, &answer["error"]), (400, &json!("bad_request")));
}

#[test]
fn an_entry_is_found_by_path() {
    let s = start_imported();
    let token = s.token();
    let path = s.put("a.note", b"abc");
    let id = id_of(&s, &path);
    let (status, by_path) = s.get(Some(&token), &format!("/entries?path={}", q(&path)));
    assert_eq!(status, 200, "{by_path}");
    let (_, by_id) = s.get(Some(&token), &format!("/entries/{id}"));
    assert_eq!(by_path, by_id);
    // A folder too.
    let (status, folder) = s.get(Some(&token), &format!("/entries?path={}", q("NOTE/Note")));
    assert_eq!(status, 200, "{folder}");
    assert_eq!(folder["kind"], "folder");
    // The token is needed.
    let (status, _) = s.get(None, &format!("/entries?path={}", q(&path)));
    assert_eq!(status, 401);
}

#[test]
fn an_unknown_or_hidden_path_is_not_found() {
    let s = start_imported();
    let token = s.token();
    hand(&s, "NOTE/Note/.hidden.note", b"x");
    for p in ["NOTE/Note/none.note", "NOTE/Note/.hidden.note", "note/Note"] {
        let (status, answer) = s.get(Some(&token), &format!("/entries?path={}", q(p)));
        assert_eq!(
            (status, answer),
            (404, json!({"error": "not_found"})),
            "{p}"
        );
    }
}

#[test]
fn a_path_that_does_not_parse_is_bad_name() {
    let s = start_imported();
    let token = s.token();
    for p in ["NOTE/../NOTE", "NOTE/./Note", ""] {
        let (status, answer) = s.get(Some(&token), &format!("/entries?path={}", q(p)));
        assert_eq!((status, &answer["error"]), (400, &json!("bad_name")), "{p}");
    }
}

#[test]
fn a_missing_or_root_path_is_bad_name() {
    let s = start_imported();
    let token = s.token();
    for url in ["/entries", "/entries?path=", "/entries?path=%2F", "/entries?other=1"] {
        let (status, answer) = s.get(Some(&token), url);
        assert_eq!(
            (status, &answer["error"]),
            (400, &json!("bad_name")),
            "{url}"
        );
    }
}

#[test]
fn a_path_through_a_hidden_folder_is_not_found() {
    let s = start_imported();
    let token = s.token();
    hand(&s, "NOTE/Note/.secret/a.note", b"x");
    for p in ["NOTE/Note/.secret", "NOTE/Note/.secret/a.note"] {
        let (status, answer) = s.get(Some(&token), &format!("/entries?path={}", q(p)));
        assert_eq!(
            (status, answer),
            (404, json!({"error": "not_found"})),
            "{p}"
        );
    }
}

#[test]
fn a_file_made_by_hand_is_found_after_the_scan() {
    let s = start_imported();
    let token = s.token();
    hand(&s, "NOTE/Note/hand.note", b"by hand");
    let (status, entry) = s.get(
        Some(&token),
        &format!("/entries?path={}", q("NOTE/Note/hand.note")),
    );
    assert_eq!(status, 200, "{entry}");
    let id = entry["id"].as_u64().unwrap();
    assert_eq!(entry["md5"], md5_hex(b"by hand"));
    let created = events(&s)
        .into_iter()
        .find(|e| e["type"] == "file.created" && e["path"] == "NOTE/Note/hand.note")
        .expect("the scan journals the file");
    assert_eq!(created["origin"], "scan");
    assert_eq!(created["id"], json!(id));
}

#[test]
fn a_download_gives_the_bytes_and_a_strong_etag() {
    let s = start_imported();
    let token = s.token();
    let bytes = b"0123456789".repeat(1000);
    let path = s.put("a.note", &bytes);
    let id = id_of(&s, &path);
    let r = get_with(&s, &token, &format!("/files/{id}"), &[]);
    assert_eq!(r.status().as_u16(), 200);
    let etag = format!("\"{}\"", md5_hex(&bytes));
    assert_eq!(header(&r, "etag"), Some(etag.clone()));
    assert_eq!(header(&r, "content-length"), Some(bytes.len().to_string()));
    assert_eq!(header(&r, "accept-ranges").as_deref(), Some("bytes"));
    assert_eq!(
        header(&r, "content-type").as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(r.bytes().unwrap().as_ref(), bytes.as_slice());
    // HEAD: the same headers, no body.
    let r = s.request("HEAD", Some(&token), &format!("/files/{id}"));
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(header(&r, "etag"), Some(etag));
    assert_eq!(header(&r, "content-length"), Some(bytes.len().to_string()));
    assert!(r.bytes().unwrap().is_empty());
}

#[test]
fn a_range_download_gives_206_and_the_part() {
    let s = start_imported();
    let token = s.token();
    let path = s.put("a.note", b"0123456789");
    let id = id_of(&s, &path);
    let etag = format!("\"{}\"", md5_hex(b"0123456789"));
    let before = s.log_lines().len();
    let r = get_with(
        &s,
        &token,
        &format!("/files/{id}"),
        &[("Range", "bytes=2-5")],
    );
    assert_eq!(r.status().as_u16(), 206);
    assert_eq!(header(&r, "content-range").as_deref(), Some("bytes 2-5/10"));
    assert_eq!(header(&r, "content-length").as_deref(), Some("4"));
    assert_eq!(header(&r, "etag"), Some(etag.clone()));
    assert_eq!(r.bytes().unwrap().as_ref(), b"2345");
    let r = get_with(
        &s,
        &token,
        &format!("/files/{id}"),
        &[("Range", "bytes=-3")],
    );
    assert_eq!(r.status().as_u16(), 206);
    assert_eq!(header(&r, "content-range").as_deref(), Some("bytes 7-9/10"));
    assert_eq!(r.bytes().unwrap().as_ref(), b"789");
    // The current ETag in If-Range keeps the range.
    let r = get_with(
        &s,
        &token,
        &format!("/files/{id}"),
        &[("Range", "bytes=8-"), ("If-Range", &etag)],
    );
    assert_eq!(r.status().as_u16(), 206);
    assert_eq!(r.bytes().unwrap().as_ref(), b"89");
    // A successful download is not logged, a part neither.
    assert_eq!(s.log_lines().len(), before);
}

#[test]
fn an_unsatisfiable_range_is_416() {
    let s = start_imported();
    let token = s.token();
    let path = s.put("a.note", b"0123456789");
    let id = id_of(&s, &path);
    let r = get_with(
        &s,
        &token,
        &format!("/files/{id}"),
        &[("Range", "bytes=10-")],
    );
    assert_eq!(r.status().as_u16(), 416);
    assert_eq!(header(&r, "content-range").as_deref(), Some("bytes */10"));
}

#[test]
fn if_range_with_an_old_etag_sends_the_whole_file() {
    let s = start_imported();
    let token = s.token();
    let path = s.put("a.note", b"old bytes");
    let id = id_of(&s, &path);
    let old = format!("\"{}\"", md5_hex(b"old bytes"));
    s.put("a.note", b"new bytes!");
    assert_eq!(id_of(&s, &path), id);
    let new = format!("\"{}\"", md5_hex(b"new bytes!"));
    for if_range in [
        old.as_str(),
        &format!("W/{new}"),
        "Sat, 01 Jan 2000 00:00:00 GMT",
    ] {
        let r = get_with(
            &s,
            &token,
            &format!("/files/{id}"),
            &[("Range", "bytes=0-2"), ("If-Range", if_range)],
        );
        assert_eq!(r.status().as_u16(), 200, "{if_range}");
        assert!(header(&r, "content-range").is_none());
        assert_eq!(header(&r, "etag"), Some(new.clone()));
        assert_eq!(r.bytes().unwrap().as_ref(), b"new bytes!");
    }
}

#[test]
fn a_download_etag_gives_304() {
    let s = start_imported();
    let token = s.token();
    let path = s.put("a.note", b"abc");
    let id = id_of(&s, &path);
    let etag = format!("\"{}\"", md5_hex(b"abc"));
    let before = s.log_lines().len();
    for inm in [etag.clone(), format!("\"x\", {etag}"), "*".to_string()] {
        let r = get_with(
            &s,
            &token,
            &format!("/files/{id}"),
            &[("If-None-Match", &inm)],
        );
        assert_eq!(r.status().as_u16(), 304, "{inm}");
        assert_eq!(header(&r, "etag"), Some(etag.clone()));
        assert!(r.bytes().unwrap().is_empty());
    }
    assert_eq!(s.log_lines().len(), before, "a 304 is not logged");
    let r = get_with(
        &s,
        &token,
        &format!("/files/{id}"),
        &[("If-None-Match", "\"other\"")],
    );
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.bytes().unwrap().as_ref(), b"abc");
}

#[test]
fn a_download_of_a_folder_is_409() {
    let s = start_imported();
    let token = s.token();
    let id = id_of(&s, "NOTE/Note");
    let (status, answer) = s.get(Some(&token), &format!("/files/{id}"));
    assert_eq!((status, answer), (409, json!({"error": "is_a_folder"})));
    let r = s.request("HEAD", Some(&token), &format!("/files/{id}"));
    assert_eq!(r.status().as_u16(), 409);
}

#[test]
fn an_unknown_id_is_404() {
    let s = start_imported();
    let token = s.token();
    for p in ["/entries/999999", "/files/999999", "/files/0"] {
        let (status, answer) = s.get(Some(&token), p);
        assert_eq!(
            (status, answer),
            (404, json!({"error": "not_found"})),
            "{p}"
        );
    }
    let (status, answer) = s.get(Some(&token), "/files/x");
    assert_eq!((status, &answer["error"]), (400, &json!("bad_request")));
}

#[test]
fn a_download_during_a_replace_gives_the_old_bytes_whole() {
    let s = start_imported();
    let token = s.token();
    let old: Vec<u8> = (0..8 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let path = s.put("big.note", &old);
    let id = id_of(&s, &path);
    let mut r = get_with(&s, &token, &format!("/files/{id}"), &[]);
    assert_eq!(r.status().as_u16(), 200);
    let mut first = vec![0u8; 1024];
    r.read_exact(&mut first).unwrap();
    s.put("big.note", b"the new bytes");
    assert_eq!(id_of(&s, &path), id);
    let mut rest = Vec::new();
    r.read_to_end(&mut rest).unwrap();
    first.extend(rest);
    assert_eq!(first.len(), old.len());
    assert!(first == old, "the old bytes, whole");
    assert_eq!(header(&r, "etag"), Some(format!("\"{}\"", md5_hex(&old))));
    // The next download has the new bytes.
    let r = get_with(&s, &token, &format!("/files/{id}"), &[]);
    assert_eq!(r.bytes().unwrap().as_ref(), b"the new bytes");
}

#[test]
fn a_removed_account_token_cannot_browse() {
    let s = start_imported();
    let token = s.token();
    let old = Accounts::new(&s.data).get("jordi").unwrap().unwrap();
    let path = s.put("a.note", b"a");
    let id = id_of(&s, &path);
    let accounts = s.state.auth.accounts();
    accounts.remove("jordi", &s.state.journal).unwrap();
    accounts.create("jordi", EMAIL, PASSWORD).unwrap();
    for p in [
        "/tree".to_string(),
        format!("/entries/{id}"),
        format!("/files/{id}"),
    ] {
        let (status, body) = s.get(Some(&token), &p);
        assert_eq!(status, 410, "{p}");
        assert_eq!(body["error"], "account_removed");
        assert_eq!(body["userId"], old.user_id);
    }
    let r = s.request("HEAD", Some(&token), &format!("/files/{id}"));
    assert_eq!(r.status().as_u16(), 410);
}

#[test]
fn a_hidden_file_is_never_listed_or_served() {
    let s = start_imported();
    let token = s.token();
    hand(&s, "NOTE/Note/.hidden.note", b"h");
    hand(&s, "NOTE/Note/.dir/x.note", b"x");
    // Another file, so the account is not emptied (a scan would hold).
    s.put("keep.note", b"k");
    let path = s.put("a.note", b"a");
    let id = id_of(&s, &path);
    let (_, entries) = tree_all(&s, &token, "recursive=true");
    let answer = json!({ "entries": entries });
    assert!(paths(&answer).iter().all(|p| !p.contains("/.")), "{answer}");
    for p in ["NOTE/Note/.dir", "NOTE/Note/.hidden.note"] {
        let (status, answer) = s.get(Some(&token), &format!("/tree?path={}", q(p)));
        assert_eq!(
            (status, answer),
            (404, json!({"error": "not_found"})),
            "{p}"
        );
    }
    // A listed file renamed by hand to a dot name is gone, never served under its old id.
    std::fs::rename(files(&s).join(&path), files(&s).join("NOTE/Note/.a.note")).unwrap();
    for p in [format!("/entries/{id}"), format!("/files/{id}")] {
        let (status, answer) = s.get(Some(&token), &p);
        assert_eq!(
            (status, answer),
            (404, json!({"error": "not_found"})),
            "{p}"
        );
    }
}

#[test]
fn a_read_token_can_browse() {
    let s = start_imported();
    let path = s.put("a.note", b"abc");
    let id = id_of(&s, &path);
    for scope in [
        json!({}),
        json!({"scope": "read"}),
        json!({"scope": "write"}),
    ] {
        let (status, login) = s.login_extra(scope.clone());
        assert_eq!(status, 201, "{login}");
        let token = login["token"].as_str().unwrap();
        assert_eq!(s.get(Some(token), "/tree").0, 200, "{scope}");
        assert_eq!(s.get(Some(token), &format!("/entries/{id}")).0, 200);
        let r = get_with(&s, token, &format!("/files/{id}"), &[]);
        assert_eq!(r.status().as_u16(), 200);
        assert_eq!(r.bytes().unwrap().as_ref(), b"abc");
    }
    let (status, _) = json_of(s.request("GET", None, "/tree"));
    assert_eq!(status, 401);
}

// ------------------------------------------------------------------ upload

/// The events after the first `n`.
fn events_after(s: &Server, n: usize) -> Vec<Value> {
    events(s).into_iter().skip(n).collect()
}

fn types(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| e["type"].as_str().unwrap().to_string())
        .collect()
}

/// The staging folders of the app's uploads that are left.
fn app_staging(s: &Server) -> Vec<String> {
    match std::fs::read_dir(s.data.join("accounts/jordi/staging")) {
        Ok(rd) => rd
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("app-"))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Waits (at most 10 s) until no staging folder of an app upload is left.
fn wait_for_clean_staging(s: &Server) {
    for _ in 0..200 {
        if app_staging(s).is_empty() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("staging left: {:?}", app_staging(s));
}

/// A raw HTTP/1.1 request to the server: `head` (request line and headers, without the empty
/// line), then `body`. With `shutdown` the write side is closed after the body. Returns the
/// status (0 when the server answered nothing) and the whole answer text.
fn raw(s: &Server, head: &str, body: &[u8], shutdown: bool) -> (u16, String) {
    use std::io::Write;
    let addr = s.base.trim_start_matches("http://");
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    stream
        .write_all(format!("{head}\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes())
        .unwrap();
    stream.write_all(body).unwrap();
    stream.flush().unwrap();
    if shutdown {
        let _ = stream.shutdown(std::net::Shutdown::Write);
    }
    let mut text = Vec::new();
    let _ = stream.read_to_end(&mut text);
    let text = String::from_utf8_lossy(&text).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, text)
}

fn upload_head(path: &str, token: &str, extra: &str) -> String {
    format!(
        "PUT /rest/v1/files?path={} HTTP/1.1\r\nAuthorization: Bearer {token}{extra}",
        q(path)
    )
}

#[test]
fn an_upload_creates_a_file_with_the_app_origin() {
    let s = start_imported();
    let t = writer(&s);
    let n = events(&s).len();
    let (status, answer) = upload(&s, &t, "NOTE/Note/New/a.txt", b"hello");
    assert_eq!(status, 201, "{answer}");
    assert_eq!(answer["path"], "NOTE/Note/New/a.txt");
    assert_eq!(answer["outcome"], "created");
    assert_eq!(answer["md5"], md5_hex(b"hello"));
    assert_eq!(answer["size"], 5);
    assert!(answer["mtimeMs"].as_u64().unwrap() > 0, "{answer}");
    assert_eq!(answer["id"], id_of(&s, "NOTE/Note/New/a.txt"));
    let ev = events_after(&s, n);
    assert_eq!(types(&ev), ["folder.created", "file.created"]);
    assert_eq!(ev[0]["path"], "NOTE/Note/New");
    assert_eq!(ev[1]["path"], "NOTE/Note/New/a.txt");
    for e in &ev {
        assert_eq!(e["origin"], "app", "{e}");
        assert!(e["device"].is_null(), "{e}");
    }
    let seqs: Vec<u64> = ev.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert_eq!(answer["seqs"], json!(seqs));
    assert_eq!(
        std::fs::read(files(&s).join("NOTE/Note/New/a.txt")).unwrap(),
        b"hello"
    );
    assert!(app_staging(&s).is_empty());
}

#[test]
fn an_upload_of_a_note_writes_its_page_events() {
    let s = start_imported();
    let t = writer(&s);
    let note = std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/notes/test1.note"),
    )
    .unwrap();
    let n = events(&s).len();
    let (status, answer) = upload(&s, &t, "NOTE/Note/t.note", &note);
    assert_eq!(status, 201, "{answer}");
    let ev = events_after(&s, n);
    assert_eq!(ev[0]["type"], "file.created");
    assert!(ev.len() > 1, "{ev:?}");
    assert!(ev[1..].iter().all(|e| e["type"] == "page.added"), "{ev:?}");
    assert!(ev.iter().all(|e| e["origin"] == "app"));
}

#[test]
fn a_replace_keeps_the_id_and_recycles_the_old_bytes() {
    let s = start_imported();
    let t = writer(&s);
    let (status, first) = upload(&s, &t, "NOTE/Note/a.txt", b"one");
    assert_eq!(status, 201, "{first}");
    let n = events(&s).len();
    let (status, second) = upload(&s, &t, "NOTE/Note/a.txt", b"two");
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["outcome"], "replaced");
    assert_eq!(second["id"], first["id"]);
    let ev = events_after(&s, n);
    assert_eq!(types(&ev), ["file.modified"]);
    assert_eq!(ev[0]["prevMd5"], md5_hex(b"one"));
    assert_eq!(ev[0]["origin"], "app");
    let store = s.state.store("jordi").unwrap();
    let g = store.lock().unwrap();
    let recycled = store.recycle_index(&g).unwrap();
    assert_eq!(recycled.len(), 1);
    assert_eq!(recycled[0].md5, md5_hex(b"one"));
    assert_eq!(
        std::fs::read(store.recycle_path(&recycled[0]).unwrap()).unwrap(),
        b"one"
    );
}

#[test]
fn identical_bytes_change_nothing() {
    let s = start_imported();
    let t = writer(&s);
    let (status, first) = upload(&s, &t, "NOTE/Note/a.txt", b"one");
    assert_eq!(status, 201, "{first}");
    let n = events(&s).len();
    let (status, again) = upload(&s, &t, "NOTE/Note/a.txt", b"one");
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["outcome"], "unchanged");
    assert_eq!(again["id"], first["id"]);
    assert_eq!(again["seqs"], json!([]));
    assert_eq!(again["md5"], md5_hex(b"one"));
    assert_eq!(again["size"], 3);
    assert_eq!(events(&s).len(), n);
}

#[test]
fn a_case_or_normalization_collision_is_refused() {
    let s = start_imported();
    let t = writer(&s);
    assert_eq!(upload(&s, &t, "NOTE/Note/Work/a.txt", b"a").0, 201);
    assert_eq!(upload(&s, &t, "NOTE/Note/cafe\u{301}.txt", b"c").0, 201);
    let n = events(&s).len();
    let (status, answer) = upload(&s, &t, "NOTE/Note/Work/A.txt", b"x");
    assert_eq!(status, 409, "{answer}");
    assert_eq!(answer, json!({"error": "name_taken", "existing": "a.txt"}));
    let (status, answer) = upload(&s, &t, "NOTE/Note/work/b.txt", b"x");
    assert_eq!(status, 409, "{answer}");
    assert_eq!(answer, json!({"error": "name_taken", "existing": "Work"}));
    let (status, answer) = upload(&s, &t, "NOTE/Note/caf\u{e9}.txt", b"x");
    assert_eq!(status, 409, "{answer}");
    assert_eq!(answer["error"], "name_taken");
    assert_eq!(events(&s).len(), n);
    assert!(app_staging(&s).is_empty());
}

#[test]
fn an_upload_outside_the_trees_is_refused() {
    let s = start_imported();
    let t = writer(&s);
    for path in [
        "NOTE/Other/a.txt",
        "a.txt",
        "NOTE/a.txt",
        "NOTE/Note",
        "note/Note/a.txt",
    ] {
        let (status, answer) = upload(&s, &t, path, b"x");
        assert_eq!(status, 403, "{path}: {answer}");
        assert_eq!(answer, json!({"error": "not_writable"}), "{path}");
    }
    assert!(!files(&s).join("NOTE/Other").exists());
}

#[test]
fn an_upload_onto_a_folder_is_refused() {
    let s = start_imported();
    let t = writer(&s);
    assert_eq!(upload(&s, &t, "NOTE/Note/Work/a.txt", b"a").0, 201);
    let (status, answer) = upload(&s, &t, "NOTE/Note/Work", b"x");
    assert_eq!(status, 409, "{answer}");
    assert_eq!(answer, json!({"error": "is_a_folder"}));
    assert!(files(&s).join("NOTE/Note/Work").is_dir());
}

#[test]
fn an_upload_below_a_file_is_refused() {
    let s = start_imported();
    let t = writer(&s);
    assert_eq!(upload(&s, &t, "NOTE/Note/a.txt", b"a").0, 201);
    let (status, answer) = upload(&s, &t, "NOTE/Note/a.txt/b.txt", b"x");
    assert_eq!(status, 409, "{answer}");
    assert_eq!(answer, json!({"error": "not_a_folder"}));
}

#[test]
fn bad_names_are_refused() {
    let s = start_imported();
    let t = writer(&s);
    let long = "x".repeat(256);
    for (path, status, code) in [
        ("NOTE/Note/../a.txt", 400, "bad_name"),
        ("NOTE/Note/./a.txt", 400, "bad_name"),
        ("NOTE/Note/.hidden", 400, "bad_name"),
        ("NOTE/Note/.dir/a.txt", 400, "bad_name"),
        ("", 400, "bad_name"),
        (&format!("NOTE/Note/{long}"), 400, "name_too_long"),
    ] {
        let (got, answer) = upload(&s, &t, path, b"x");
        assert_eq!(got, status, "{path}: {answer}");
        assert_eq!(answer, json!({"error": code}), "{path}");
    }
    // 255 bytes is fine.
    let ok = "y".repeat(255);
    assert_eq!(upload(&s, &t, &format!("NOTE/Note/{ok}"), b"x").0, 201);
    // No path at all.
    let r = s
        .client
        .put(s.url("/files"))
        .bearer_auth(&t)
        .body("x")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);
}

#[test]
fn upload_limits() {
    let s = start_imported();
    let t = writer(&s);
    s.state.set_upload_limits(10, 10);
    // No Content-Length (a chunked body): 411.
    let (status, text) = raw(
        &s,
        &upload_head("NOTE/Note/a.txt", &t, "\r\nTransfer-Encoding: chunked"),
        b"",
        false,
    );
    assert_eq!(status, 411, "{text}");
    assert!(text.contains("length_required"), "{text}");
    // Over the limit: 413 from the headers alone, the body is never sent.
    let (status, text) = raw(
        &s,
        &upload_head("NOTE/Note/a.txt", &t, "\r\nContent-Length: 11"),
        b"",
        false,
    );
    assert_eq!(status, 413, "{text}");
    assert!(text.contains("too_large"), "{text}");
    // At the limit: fine.
    assert_eq!(upload(&s, &t, "NOTE/Note/a.txt", &[b'z'; 10]).0, 201);
    assert!(app_staging(&s).is_empty());
}

#[test]
fn a_short_body_is_refused() {
    let s = start_imported();
    let t = writer(&s);
    let n = events(&s).len();
    let (status, text) = raw(
        &s,
        &upload_head("NOTE/Note/a.txt", &t, "\r\nContent-Length: 10"),
        b"12345",
        true,
    );
    assert_eq!(status, 400, "{text}");
    assert!(text.contains("bad_length"), "{text}");
    wait_for_clean_staging(&s);
    assert_eq!(events(&s).len(), n);
    assert!(!files(&s).join("NOTE/Note/a.txt").exists());
}

#[test]
fn a_wrong_md5_header_is_refused() {
    let s = start_imported();
    let t = writer(&s);
    let n = events(&s).len();
    let (status, answer) = upload_with(
        &s,
        &t,
        "NOTE/Note/a.txt",
        b"abc",
        &[("X-Notedown-Md5", &md5_hex(b"abd"))],
    );
    assert_eq!(status, 400, "{answer}");
    assert_eq!(answer, json!({"error": "bad_md5"}));
    assert_eq!(events(&s).len(), n);
    assert!(app_staging(&s).is_empty());
    let (status, answer) = upload_with(
        &s,
        &t,
        "NOTE/Note/a.txt",
        b"abc",
        &[("X-Notedown-Md5", &md5_hex(b"abc").to_uppercase())],
    );
    assert_eq!(status, 201, "{answer}");
}

#[test]
fn a_broken_upload_changes_nothing() {
    let s = start_imported();
    let t = writer(&s);
    let n = events(&s).len();
    {
        use std::io::Write;
        let addr = s.base.trim_start_matches("http://");
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        let head = upload_head("NOTE/Note/big.bin", &t, "\r\nContent-Length: 2000000");
        stream
            .write_all(format!("{head}\r\nHost: {addr}\r\n\r\n").as_bytes())
            .unwrap();
        stream.write_all(&vec![7u8; 1_000_000]).unwrap();
        stream.flush().unwrap();
        // Let the server stage some of it, then cut the connection.
        let mut staged = false;
        for _ in 0..250 {
            staged = !app_staging(&s).is_empty();
            if staged {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(staged, "the upload never reached staging");
    }
    wait_for_clean_staging(&s);
    assert_eq!(events(&s).len(), n);
    assert!(!files(&s).join("NOTE/Note/big.bin").exists());
}

#[test]
fn if_none_match_refuses_an_existing_file() {
    let s = start_imported();
    let t = writer(&s);
    assert_eq!(upload(&s, &t, "NOTE/Note/a.txt", b"one").0, 201);
    let n = events(&s).len();
    let (status, answer) =
        upload_with(&s, &t, "NOTE/Note/a.txt", b"two", &[("If-None-Match", "*")]);
    assert_eq!(status, 412, "{answer}");
    assert_eq!(answer, json!({"error": "precondition_failed"}));
    assert_eq!(events(&s).len(), n);
    assert_eq!(
        std::fs::read(files(&s).join("NOTE/Note/a.txt")).unwrap(),
        b"one"
    );
    // A new path passes.
    let (status, _) = upload_with(&s, &t, "NOTE/Note/b.txt", b"two", &[("If-None-Match", "*")]);
    assert_eq!(status, 201);
}

#[test]
fn if_match_refuses_a_changed_file() {
    let s = start_imported();
    let t = writer(&s);
    assert_eq!(upload(&s, &t, "NOTE/Note/a.txt", b"one").0, 201);
    let n = events(&s).len();
    let stale = format!("\"{}\"", md5_hex(b"zero"));
    let (status, answer) = upload_with(&s, &t, "NOTE/Note/a.txt", b"two", &[("If-Match", &stale)]);
    assert_eq!(status, 412, "{answer}");
    assert_eq!(answer, json!({"error": "precondition_failed"}));
    // A weak tag never matches.
    let weak = format!("W/\"{}\"", md5_hex(b"one"));
    let (status, _) = upload_with(&s, &t, "NOTE/Note/a.txt", b"two", &[("If-Match", &weak)]);
    assert_eq!(status, 412);
    // Nothing at the path meets no If-Match.
    let current = format!("\"{}\"", md5_hex(b"one"));
    let (status, _) = upload_with(&s, &t, "NOTE/Note/b.txt", b"two", &[("If-Match", &current)]);
    assert_eq!(status, 412);
    assert_eq!(events(&s).len(), n);
    let (status, answer) =
        upload_with(&s, &t, "NOTE/Note/a.txt", b"two", &[("If-Match", &current)]);
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["outcome"], "replaced");
}

#[test]
fn a_retried_upload_with_if_none_match_is_unchanged() {
    let s = start_imported();
    let t = writer(&s);
    let h = [("If-None-Match", "*")];
    let (status, first) = upload_with(&s, &t, "NOTE/Note/a.txt", b"one", &h);
    assert_eq!(status, 201, "{first}");
    let n = events(&s).len();
    let (status, again) = upload_with(&s, &t, "NOTE/Note/a.txt", b"one", &h);
    assert_eq!(status, 200, "{again}");
    assert_eq!(again["outcome"], "unchanged");
    assert_eq!(again["id"], first["id"]);
    assert_eq!(events(&s).len(), n);
}

#[test]
fn a_read_token_cannot_upload() {
    let s = start_imported();
    let t = s.token();
    let (status, answer) = upload(&s, &t, "NOTE/Note/a.txt", b"x");
    assert_eq!(status, 403, "{answer}");
    assert_eq!(answer, json!({"error": "read_only_token"}));
    let (status, _) = upload(&s, "nonsense", "NOTE/Note/a.txt", b"x");
    assert_eq!(status, 401);
    assert!(!files(&s).join("NOTE/Note/a.txt").exists());
}

#[test]
fn uploads_are_refused_in_refuse_mode() {
    let s = crate::rest_server::start_imported_with(|c| c.sync = notedown_cloud::SyncMode::Refuse);
    let t = writer(&s);
    // Only the headers are sent: an answer proves that nobody waits for the body.
    let (status, text) = raw(
        &s,
        &upload_head("NOTE/Note/a.txt", &t, "\r\nContent-Length: 100000000"),
        b"",
        false,
    );
    assert_eq!(status, 409, "{text}");
    assert!(text.contains("sync_refused"), "{text}");
    assert!(!files(&s).join("NOTE/Note/a.txt").exists());
}

#[test]
fn an_upload_waits_while_a_device_syncs() {
    let s = start_imported();
    let t = writer(&s);
    s.state
        .sync_locks
        .start("jordi", "SN078C10000000", s.state.now_ms());
    let r = s
        .client
        .put(s.url(&format!("/files?path={}", q("NOTE/Note/a.txt"))))
        .bearer_auth(&t)
        .body("x")
        .send()
        .unwrap();
    assert_eq!(header(&r, "retry-after").as_deref(), Some("5"));
    let (status, answer) = json_of(r);
    assert_eq!(status, 423, "{answer}");
    assert_eq!(answer, json!({"error": "device_syncing"}));
    assert!(!files(&s).join("NOTE/Note/a.txt").exists());
}

#[test]
fn an_upload_over_the_quota_is_refused() {
    let s = start_imported();
    let t = writer(&s);
    let accounts = Accounts::new(&s.data);
    let mut a = accounts.get("jordi").unwrap().unwrap();
    a.quota_bytes = Some(10);
    accounts.save(&a).unwrap();
    assert_eq!(upload(&s, &t, "NOTE/Note/a.txt", b"12345678").0, 201);
    s.state.invalidate_usage("jordi");
    let n = events(&s).len();
    let (status, answer) = upload(&s, &t, "NOTE/Note/b.txt", b"123");
    assert_eq!(status, 507, "{answer}");
    assert_eq!(answer, json!({"error": "quota_exceeded"}));
    assert_eq!(events(&s).len(), n);
    // A replace counts only what it adds.
    let (status, answer) = upload(&s, &t, "NOTE/Note/a.txt", b"1234567890");
    assert_eq!(status, 200, "{answer}");
    assert!(app_staging(&s).is_empty());
}

/// Two writers of one path at once: each commits whole, one after the other.
fn both_commit_whole(s: &Server, other: impl FnOnce() + Send) {
    let t = writer(s);
    let a = vec![b'a'; 3_000_000];
    let n = events(s).len();
    std::thread::scope(|scope| {
        let h = scope.spawn(other);
        let (status, answer) = upload(s, &t, "NOTE/Note/x.bin", &a);
        assert!(status == 200 || status == 201, "{answer}");
        h.join().unwrap();
    });
    let now = std::fs::read(files(s).join("NOTE/Note/x.bin")).unwrap();
    assert!(now == a || now == vec![b'b'; 3_000_000], "mixed bytes");
    let ev = events_after(s, n);
    let mut kinds = types(&ev);
    kinds.sort();
    assert_eq!(kinds, ["file.created", "file.modified"], "{ev:?}");
    assert_eq!(ev[1]["md5"], md5_hex(&now));
}

#[test]
fn two_rest_uploads_of_one_path_both_commit_whole() {
    let s = start_imported();
    let t2 = writer(&s);
    let s2 = &s;
    both_commit_whole(&s, move || {
        let (status, answer) = upload(s2, &t2, "NOTE/Note/x.bin", &vec![b'b'; 3_000_000]);
        assert!(status == 200 || status == 201, "{answer}");
    });
}

#[test]
fn a_rest_upload_and_a_cloud_put_of_one_path_both_commit_whole() {
    let s = start_imported();
    let src = s.data.parent().unwrap().join("x.bin");
    std::fs::write(&src, vec![b'b'; 3_000_000]).unwrap();
    let s2 = &s;
    both_commit_whole(&s, move || {
        put::put(
            &s2.data,
            "jordi",
            &s2.state.journal,
            &src,
            "NOTE/Note",
            None,
        )
        .unwrap();
    });
}

#[cfg(unix)]
#[test]
fn a_symlinked_folder_is_never_written_through() {
    let s = start_imported();
    let t = writer(&s);
    let outside = s.data.parent().unwrap().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, files(&s).join("NOTE/Note/link")).unwrap();
    std::os::unix::fs::symlink(outside.join("f.txt"), files(&s).join("NOTE/Note/f.txt")).unwrap();
    let n = events(&s).len();
    // Pinned: refused, never created in place of the link nor written through it.
    let (status, answer) = upload(&s, &t, "NOTE/Note/link/a.txt", b"x");
    assert_eq!(status, 409, "{answer}");
    assert_eq!(answer, json!({"error": "not_a_folder"}));
    let (status, answer) = upload(&s, &t, "NOTE/Note/link/deeper/a.txt", b"x");
    assert_eq!(status, 409, "{answer}");
    assert_eq!(answer, json!({"error": "not_a_folder"}));
    let (status, answer) = upload(&s, &t, "NOTE/Note/f.txt", b"x");
    assert_eq!(status, 409, "{answer}");
    assert_eq!(answer, json!({"error": "name_taken", "existing": "f.txt"}));
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    assert!(
        std::fs::symlink_metadata(files(&s).join("NOTE/Note/link"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(events(&s).len(), n);
    assert!(app_staging(&s).is_empty());
}

#[test]
fn the_upload_body_is_not_logged() {
    let s = start_imported();
    let t = writer(&s);
    let (status, _) = upload(&s, &t, "NOTE/Note/a.txt", b"SECRET-BODY-BYTES");
    assert_eq!(status, 201);
    let entries = upload_log(&s);
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0]["request_body"], "<17 bytes, not logged>");
    assert!(
        !s.log_lines()
            .iter()
            .any(|l| l.contains("SECRET-BODY-BYTES"))
    );
    // A refused one is logged too, and its body neither.
    s.state.set_upload_limits(4, 4);
    let (status, _) = upload(&s, &t, "NOTE/Note/b.txt", b"SECRET-BODY-BYTES");
    assert_eq!(status, 413);
    let entries = upload_log(&s);
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert_eq!(entries[1]["response_status"], 413);
    assert!(
        !s.log_lines()
            .iter()
            .any(|l| l.contains("SECRET-BODY-BYTES"))
    );
}

/// The log entries of the uploads, in order.
fn upload_log(s: &Server) -> Vec<Value> {
    s.log_lines()
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e["method"] == "PUT" && e["path"] == "/rest/v1/files")
        .collect()
}

#[test]
fn a_json_body_over_8_mib_gets_a_json_413() {
    let s = start_imported();
    let t = writer(&s);
    let big = vec![b' '; notedown_cloud::server::MAX_REQUEST_BODY + 1];
    let r = s
        .client
        .post(s.url("/login"))
        .bearer_auth(&t)
        .header("Content-Type", "application/json")
        .body(big)
        .send()
        .unwrap();
    let (status, answer) = json_of(r);
    assert_eq!(status, 413, "{answer}");
    assert_eq!(answer, json!({"error": "too_large"}));
}

/// An upload of `total` bytes to `path` on a raw connection that has sent only `first` so far,
/// once its staging folder is there (so every check before the body has passed).
fn partial_upload(
    s: &Server,
    token: &str,
    path: &str,
    total: usize,
    first: &[u8],
) -> std::net::TcpStream {
    use std::io::Write;
    let addr = s.base.trim_start_matches("http://");
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let head = upload_head(path, token, &format!("\r\nContent-Length: {total}"));
    stream
        .write_all(format!("{head}\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes())
        .unwrap();
    stream.write_all(first).unwrap();
    stream.flush().unwrap();
    for _ in 0..250 {
        if !app_staging(s).is_empty() {
            return stream;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("the upload never reached staging");
}

/// Sends the rest of a [`partial_upload`]: (status, answer text).
fn finish_upload(mut stream: std::net::TcpStream, rest: &[u8]) -> (u16, String) {
    use std::io::Write;
    stream.write_all(rest).unwrap();
    stream.flush().unwrap();
    let mut text = Vec::new();
    let _ = stream.read_to_end(&mut text);
    let text = String::from_utf8_lossy(&text).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, text)
}

#[test]
fn a_removed_account_token_cannot_upload() {
    let s = start_imported();
    let t = writer(&s);
    let accounts = s.state.auth.accounts();
    accounts.remove("jordi", &s.state.journal).unwrap();
    accounts.create("jordi", EMAIL, PASSWORD).unwrap();
    let (status, answer) = upload(&s, &t, "NOTE/Note/a.txt", b"x");
    assert_eq!(status, 410, "{answer}");
    assert_eq!(answer["error"], "account_removed");
    assert!(!files(&s).join("NOTE/Note/a.txt").exists());
}

#[test]
fn uploads_wait_while_a_scan_holds_the_account() {
    let s = start_imported();
    let t = writer(&s);
    let read = s.token();
    let gone = s.put("gone.note", b"g");
    let note_tree = format!("/tree?path={}", q("NOTE/Note"));
    // Before the body: only the headers are sent.
    std::fs::remove_file(files(&s).join(&gone)).unwrap();
    assert_eq!(s.get(Some(&read), &note_tree).0, 423);
    let (status, text) = raw(
        &s,
        &upload_head("NOTE/Note/a.txt", &t, "\r\nContent-Length: 100000000"),
        b"",
        false,
    );
    assert_eq!(status, 423, "{text}");
    assert!(text.contains("scan_hold"), "{text}");
    // Lift the hold, start an upload, hold again while its body is on the way: refused under
    // the lock.
    // The file is back and the hold lifted (as `cloud scan --accept` would).
    hand(&s, &gone, b"g");
    std::fs::remove_file(s.data.join("accounts/jordi/scan-hold.json")).unwrap();
    assert_eq!(s.get(Some(&read), &note_tree).0, 200);
    let n = events(&s).len();
    let stream = partial_upload(&s, &t, "NOTE/Note/a.txt", 6, b"abc");
    std::fs::remove_file(files(&s).join(&gone)).unwrap();
    assert_eq!(s.get(Some(&read), &note_tree).0, 423);
    let (status, text) = finish_upload(stream, b"def");
    assert_eq!(status, 423, "{text}");
    assert!(text.contains("scan_hold"), "{text}");
    wait_for_clean_staging(&s);
    assert_eq!(events(&s).len(), n);
    assert!(!files(&s).join("NOTE/Note/a.txt").exists());
}

#[test]
fn uploads_wait_for_the_import_in_protect_mode() {
    // Before the body: only the headers are sent.
    let s = start();
    assert!(s.state.config.protect);
    let t = writer(&s);
    let (status, text) = raw(
        &s,
        &upload_head("NOTE/Note/a.txt", &t, "\r\nContent-Length: 100000000"),
        b"",
        false,
    );
    assert_eq!(status, 423, "{text}");
    assert!(text.contains("not_imported"), "{text}");
    drop(s);
    // The marker goes away while the body is on the way: refused under the lock.
    let s = start_imported();
    let t = writer(&s);
    let stream = partial_upload(&s, &t, "NOTE/Note/a.txt", 6, b"abc");
    let accounts = Accounts::new(&s.data);
    let mut a = accounts.get("jordi").unwrap().unwrap();
    a.imported = None;
    accounts.save(&a).unwrap();
    let (status, text) = finish_upload(stream, b"def");
    assert_eq!(status, 423, "{text}");
    assert!(text.contains("not_imported"), "{text}");
    wait_for_clean_staging(&s);
    assert!(!files(&s).join("NOTE/Note/a.txt").exists());
}

#[test]
fn the_quota_is_checked_again_under_the_lock() {
    let s = start_imported();
    let accounts = Accounts::new(&s.data);
    let mut a = accounts.get("jordi").unwrap().unwrap();
    a.quota_bytes = Some(10);
    accounts.save(&a).unwrap();
    let t = writer(&s);
    // Each fits alone, not both: the first passes its checks before the body, then the second
    // commits, then the first ends.
    let stream = partial_upload(&s, &t, "NOTE/Note/a.txt", 6, b"abc");
    assert_eq!(upload(&s, &t, "NOTE/Note/b.txt", b"123456").0, 201);
    let (status, text) = finish_upload(stream, b"def");
    assert_eq!(status, 507, "{text}");
    assert!(text.contains("quota_exceeded"), "{text}");
    wait_for_clean_staging(&s);
    assert!(!files(&s).join("NOTE/Note/a.txt").exists());
    // The usage the server keeps knows the second upload: refused before the body now.
    let (status, text) = raw(
        &s,
        &upload_head("NOTE/Note/c.txt", &t, "\r\nContent-Length: 6"),
        b"",
        false,
    );
    assert_eq!(status, 507, "{text}");
}

#[cfg(unix)]
#[test]
fn without_quota_bytes_there_is_no_quota_check() {
    use std::os::unix::fs::PermissionsExt;
    let s = start_imported();
    assert!(
        Accounts::new(&s.data)
            .get("jordi")
            .unwrap()
            .unwrap()
            .quota_bytes
            .is_none()
    );
    let t = writer(&s);
    // A folder no walk of the whole tree can read: an upload elsewhere never walks it.
    let locked = files(&s).join("EXPORT/locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let (status, answer) = upload(&s, &t, "NOTE/Note/big.bin", &vec![1u8; 3_000_000]);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(status, 201, "{answer}");
}

// ------------------------------------------------------- folders, move, delete

/// The seqs of `events`.
fn seqs(events: &[Value]) -> Value {
    json!(
        events
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect::<Vec<_>>()
    )
}

/// The names in the folder at the wire path `rel`, as the file system has them.
fn names_in(s: &Server, rel: &str) -> BTreeSet<String> {
    std::fs::read_dir(files(s).join(rel))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

/// The page record paths of the account.
fn record_paths(s: &Server) -> Vec<String> {
    notedown_cloud::read::records(&s.data, "jordi")
        .unwrap()
        .records
        .values()
        .map(|r| r.record.path.clone())
        .collect()
}

fn test_note() -> Vec<u8> {
    std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/notes/test1.note"),
    )
    .unwrap()
}

#[test]
fn a_new_folder_and_its_parents_are_created() {
    let s = start_imported();
    let t = writer(&s);
    let n = events(&s).len();
    let (status, answer) = mkdir(&s, &t, "NOTE/Note/A/B/C");
    assert_eq!(status, 201, "{answer}");
    assert_eq!(answer["path"], "NOTE/Note/A/B/C");
    assert_eq!(answer["id"], id_of(&s, "NOTE/Note/A/B/C"));
    let ev = events_after(&s, n);
    assert_eq!(
        types(&ev),
        ["folder.created", "folder.created", "folder.created"]
    );
    let paths: Vec<&str> = ev.iter().map(|e| e["path"].as_str().unwrap()).collect();
    assert_eq!(paths, ["NOTE/Note/A", "NOTE/Note/A/B", "NOTE/Note/A/B/C"]);
    for e in &ev {
        assert_eq!(e["origin"], "app", "{e}");
        assert!(e["device"].is_null(), "{e}");
    }
    assert_eq!(answer["seqs"], seqs(&ev));
    assert!(files(&s).join("NOTE/Note/A/B/C").is_dir());
}

#[test]
fn an_existing_folder_is_200() {
    let s = start_imported();
    let t = writer(&s);
    let (status, first) = mkdir(&s, &t, "NOTE/Note/A");
    assert_eq!(status, 201, "{first}");
    let n = events(&s).len();
    let (status, again) = mkdir(&s, &t, "NOTE/Note/A");
    assert_eq!(status, 200, "{again}");
    assert_eq!(
        again,
        json!({"id": first["id"], "path": "NOTE/Note/A", "seqs": []})
    );
    assert_eq!(events(&s).len(), n);
    // A folder made by hand is journaled by the scan first, then it is there.
    std::fs::create_dir_all(files(&s).join("NOTE/Note/Hand")).unwrap();
    let (status, hand) = mkdir(&s, &t, "NOTE/Note/Hand");
    assert_eq!(status, 200, "{hand}");
    assert_eq!(hand["seqs"], json!([]));
    let ev = events_after(&s, n);
    assert_eq!(types(&ev), ["folder.created"]);
    assert_eq!(ev[0]["origin"], "scan");
    assert_eq!(hand["id"], ev[0]["id"]);
}

#[test]
fn a_folder_beside_a_case_twin_is_refused() {
    let s = start_imported();
    let t = writer(&s);
    assert_eq!(mkdir(&s, &t, "NOTE/Note/Work").0, 201);
    let n = events(&s).len();
    for path in ["NOTE/Note/work", "NOTE/Note/WORK/sub"] {
        let (status, answer) = mkdir(&s, &t, path);
        assert_eq!(status, 409, "{path}: {answer}");
        assert_eq!(answer, json!({"error": "name_taken", "existing": "Work"}));
    }
    assert_eq!(events(&s).len(), n);
}

#[test]
fn a_folder_onto_a_file_is_refused() {
    let s = start_imported();
    let t = writer(&s);
    uploaded(&s, &t, "NOTE/Note/f.txt", b"f");
    let n = events(&s).len();
    let (status, answer) = mkdir(&s, &t, "NOTE/Note/f.txt");
    assert_eq!(status, 409, "{answer}");
    assert_eq!(answer, json!({"error": "name_taken", "existing": "f.txt"}));
    let (status, answer) = mkdir(&s, &t, "NOTE/Note/f.txt/sub");
    assert_eq!(status, 409, "{answer}");
    assert_eq!(answer, json!({"error": "not_a_folder"}));
    for (path, code) in [
        ("NOTE/Other/x", "not_writable"),
        ("NOTE/Note", "not_writable"),
        ("NOTE/Note/.x", "bad_name"),
    ] {
        let (_, answer) = mkdir(&s, &t, path);
        assert_eq!(answer["error"], code, "{path}");
    }
    assert_eq!(events(&s).len(), n);
}

#[test]
fn a_rename_keeps_the_id() {
    let s = start_imported();
    let t = writer(&s);
    let id = uploaded(&s, &t, "NOTE/Note/a.txt", b"a");
    let n = events(&s).len();
    let (status, answer) = mv(&s, &t, id, "NOTE/Note/b.txt");
    assert_eq!(status, 200, "{answer}");
    let ev = events_after(&s, n);
    assert_eq!(types(&ev), ["file.moved"]);
    assert_eq!(ev[0]["oldPath"], "NOTE/Note/a.txt");
    assert_eq!(ev[0]["path"], "NOTE/Note/b.txt");
    assert_eq!(ev[0]["origin"], "app");
    assert!(ev[0]["device"].is_null());
    assert_eq!(
        answer,
        json!({"id": id, "path": "NOTE/Note/b.txt", "seqs": seqs(&ev), "removed": []})
    );
    assert_eq!(id_of(&s, "NOTE/Note/b.txt"), id);
    assert!(!files(&s).join("NOTE/Note/a.txt").exists());
    assert_eq!(
        std::fs::read(files(&s).join("NOTE/Note/b.txt")).unwrap(),
        b"a"
    );
}

#[test]
fn a_case_only_rename_works() {
    let s = start_imported();
    let t = writer(&s);
    let file = uploaded(&s, &t, "NOTE/Note/a.txt", b"a");
    uploaded(&s, &t, "NOTE/Note/Work/w.txt", b"w");
    let folder = id_of(&s, "NOTE/Note/Work");
    let (status, answer) = mv(&s, &t, file, "NOTE/Note/A.txt");
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["path"], "NOTE/Note/A.txt");
    let (status, answer) = mv(&s, &t, folder, "NOTE/Note/WORK");
    assert_eq!(status, 200, "{answer}");
    let names = names_in(&s, "NOTE/Note");
    assert!(
        names.contains("A.txt") && !names.contains("a.txt"),
        "{names:?}"
    );
    assert!(
        names.contains("WORK") && !names.contains("Work"),
        "{names:?}"
    );
    assert_eq!(id_of(&s, "NOTE/Note/A.txt"), file);
    assert_eq!(id_of(&s, "NOTE/Note/WORK"), folder);
    assert_eq!(
        std::fs::read(files(&s).join("NOTE/Note/WORK/w.txt")).unwrap(),
        b"w"
    );
}

#[test]
fn a_move_across_trees_keeps_the_id() {
    let s = start_imported();
    let t = writer(&s);
    let id = uploaded(&s, &t, "NOTE/Note/a.pdf", b"pdf");
    let n = events(&s).len();
    let (status, answer) = mv(&s, &t, id, "DOCUMENT/Document/Sub/a.pdf");
    assert_eq!(status, 200, "{answer}");
    let ev = events_after(&s, n);
    assert_eq!(types(&ev), ["folder.created", "file.moved"]);
    assert_eq!(ev[0]["path"], "DOCUMENT/Document/Sub");
    assert!(ev.iter().all(|e| e["origin"] == "app"));
    assert_eq!(answer["id"], id);
    assert_eq!(answer["seqs"], seqs(&ev));
    assert_eq!(id_of(&s, "DOCUMENT/Document/Sub/a.pdf"), id);
    assert_eq!(
        std::fs::read(files(&s).join("DOCUMENT/Document/Sub/a.pdf")).unwrap(),
        b"pdf"
    );
}

#[test]
fn a_moved_note_keeps_its_page_records() {
    let s = start_imported();
    let t = writer(&s);
    let id = uploaded(&s, &t, "NOTE/Note/t.note", &test_note());
    let before = record_paths(&s);
    assert_eq!(before, ["NOTE/Note/t.note"]);
    let (status, answer) = mv(&s, &t, id, "NOTE/Note/Work/t.note");
    assert_eq!(status, 200, "{answer}");
    assert_eq!(record_paths(&s), ["NOTE/Note/Work/t.note"]);
}

#[test]
fn a_move_onto_an_existing_name_is_refused() {
    let s = start_imported();
    let t = writer(&s);
    let a = uploaded(&s, &t, "NOTE/Note/a.txt", b"a");
    uploaded(&s, &t, "NOTE/Note/b.txt", b"b");
    uploaded(&s, &t, "NOTE/Note/X/b.txt", b"xb");
    let n = events(&s).len();
    for (to, existing) in [
        ("NOTE/Note/b.txt", "b.txt"),
        ("NOTE/Note/B.txt", "b.txt"),
        ("NOTE/Note/X/b.txt", "b.txt"),
        ("NOTE/Note/X/B.txt", "b.txt"),
        ("NOTE/Note/X", "X"),
        ("NOTE/Note/x/a.txt", "X"),
    ] {
        let (status, answer) = mv(&s, &t, a, to);
        assert_eq!(status, 409, "{to}: {answer}");
        assert_eq!(
            answer,
            json!({"error": "name_taken", "existing": existing}),
            "{to}"
        );
    }
    let (status, answer) = mv(&s, &t, a, "NOTE/Note/b.txt/a.txt");
    assert_eq!(status, 409, "{answer}");
    assert_eq!(answer, json!({"error": "not_a_folder"}));
    assert_eq!(events(&s).len(), n);
    assert_eq!(
        std::fs::read(files(&s).join("NOTE/Note/b.txt")).unwrap(),
        b"b"
    );
}

#[test]
fn a_move_out_of_the_trees_is_refused() {
    let s = start_imported();
    let t = writer(&s);
    let a = uploaded(&s, &t, "NOTE/Note/a.txt", b"a");
    let n = events(&s).len();
    for to in [
        "NOTE/Other/a.txt",
        "a.txt",
        "NOTE/a.txt",
        "NOTE/Note",
        "EXPORT",
    ] {
        let (status, answer) = mv(&s, &t, a, to);
        assert_eq!(status, 403, "{to}: {answer}");
        assert_eq!(answer, json!({"error": "not_writable"}), "{to}");
    }
    for (to, code) in [("NOTE/Note/.a.txt", "bad_name"), ("", "bad_name")] {
        let (status, answer) = mv(&s, &t, a, to);
        assert_eq!(status, 400, "{to}: {answer}");
        assert_eq!(answer["error"], code, "{to}");
    }
    assert_eq!(events(&s).len(), n);
    assert!(files(&s).join("NOTE/Note/a.txt").exists());
}

#[test]
fn a_folder_cannot_move_into_itself() {
    let s = start_imported();
    let t = writer(&s);
    assert_eq!(mkdir(&s, &t, "NOTE/Note/W/X").0, 201);
    let w = id_of(&s, "NOTE/Note/W");
    let n = events(&s).len();
    for to in ["NOTE/Note/W/W", "NOTE/Note/W/X/W"] {
        let (status, answer) = mv(&s, &t, w, to);
        assert_eq!(status, 400, "{to}: {answer}");
        assert_eq!(answer, json!({"error": "into_itself"}));
    }
    assert_eq!(events(&s).len(), n);
}

#[test]
fn a_move_to_the_same_path_changes_nothing() {
    let s = start_imported();
    let t = writer(&s);
    let a = uploaded(&s, &t, "NOTE/Note/a.txt", b"a");
    let n = events(&s).len();
    let (status, answer) = mv(&s, &t, a, "NOTE/Note/a.txt");
    assert_eq!(status, 200, "{answer}");
    assert_eq!(
        answer,
        json!({"id": a, "path": "NOTE/Note/a.txt", "seqs": [], "removed": []})
    );
    assert_eq!(events(&s).len(), n);
}

#[test]
fn a_move_that_empties_a_folder_reports_it_removed() {
    let s = start_imported();
    let t = writer(&s);
    let a = uploaded(&s, &t, "NOTE/Note/P/Q/a.txt", b"a");
    let (p, q) = (id_of(&s, "NOTE/Note/P"), id_of(&s, "NOTE/Note/P/Q"));
    let n = events(&s).len();
    let (status, answer) = mv(&s, &t, a, "NOTE/Note/a.txt");
    assert_eq!(status, 200, "{answer}");
    let ev = events_after(&s, n);
    assert_eq!(
        types(&ev),
        ["file.moved", "folder.deleted", "folder.deleted"]
    );
    assert_eq!(
        answer["removed"],
        json!([{"id": q, "path": "NOTE/Note/P/Q"}, {"id": p, "path": "NOTE/Note/P"}])
    );
    assert!(!files(&s).join("NOTE/Note/P").exists());
}

#[test]
fn a_source_outside_the_trees_cannot_be_moved_or_deleted() {
    let s = start_imported();
    let t = writer(&s);
    hand(&s, "NOTE/Other/x.txt", b"x");
    // The listing journals the file and gives it its id.
    let (status, listed) = s.get(Some(&t), &format!("/tree?path={}", q("NOTE/Other")));
    assert_eq!(status, 200, "{listed}");
    let id = find(&listed, "NOTE/Other/x.txt")["id"].as_u64().unwrap();
    let n = events(&s).len();
    let (status, answer) = mv(&s, &t, id, "NOTE/Note/x.txt");
    assert_eq!(status, 403, "{answer}");
    assert_eq!(answer, json!({"error": "not_writable"}));
    let (status, answer) = mv(&s, &t, id, "NOTE/Other/x.txt");
    assert_eq!(status, 403, "{answer}");
    let (status, answer) = delete(&s, &t, id);
    assert_eq!(status, 403, "{answer}");
    assert_eq!(answer, json!({"error": "not_writable"}));
    assert_eq!(events(&s).len(), n);
    assert!(files(&s).join("NOTE/Other/x.txt").exists());
}

#[test]
fn a_tree_root_cannot_be_renamed_moved_or_deleted() {
    let s = start_imported();
    let t = writer(&s);
    let n = events(&s).len();
    for root in [
        "NOTE/Note",
        "NOTE/MyStyle",
        "DOCUMENT/Document",
        "EXPORT",
        "SCREENSHOT",
        "INBOX",
        "NOTE",
        "DOCUMENT",
    ] {
        let id = id_of(&s, root);
        for to in ["EXPORT/Renamed", root] {
            let (status, answer) = mv(&s, &t, id, to);
            assert_eq!(status, 403, "{root} to {to}: {answer}");
            assert_eq!(answer, json!({"error": "not_writable"}), "{root}");
        }
        let (status, answer) = delete(&s, &t, id);
        assert_eq!(status, 403, "{root}: {answer}");
        assert_eq!(answer, json!({"error": "not_writable"}), "{root}");
        assert!(files(&s).join(root).is_dir());
    }
    assert_eq!(events(&s).len(), n);
}

#[test]
fn a_delete_moves_the_file_to_the_recycle_bin() {
    let s = start_imported();
    let t = writer(&s);
    let id = uploaded(&s, &t, "NOTE/Note/a.txt", b"bytes");
    let n = events(&s).len();
    let (status, answer) = delete(&s, &t, id);
    assert_eq!(status, 200, "{answer}");
    let ev = events_after(&s, n);
    assert_eq!(types(&ev), ["file.deleted"]);
    assert_eq!(ev[0]["origin"], "app");
    assert_eq!(ev[0]["id"], id);
    assert_eq!(
        answer,
        json!({"id": id, "path": "NOTE/Note/a.txt", "seqs": seqs(&ev), "removed": []})
    );
    let store = s.state.store("jordi").unwrap();
    let g = store.lock().unwrap();
    let recycled = store.recycle_index(&g).unwrap();
    assert_eq!(recycled.len(), 1);
    assert_eq!(recycled[0].id, id);
    assert_eq!(serde_json::to_value(recycled[0].reason).unwrap(), "deleted");
    assert_eq!(ev[0]["recycle"], recycled[0].location());
    assert_eq!(
        std::fs::read(store.recycle_path(&recycled[0]).unwrap()).unwrap(),
        b"bytes"
    );
    assert!(!files(&s).join("NOTE/Note/a.txt").exists());
}

#[test]
fn a_folder_delete_takes_its_content() {
    let s = start_imported();
    let t = writer(&s);
    let a = uploaded(&s, &t, "NOTE/Note/W/a.txt", b"a");
    let b = uploaded(&s, &t, "NOTE/Note/W/S/b.txt", b"b");
    let (w, sub) = (id_of(&s, "NOTE/Note/W"), id_of(&s, "NOTE/Note/W/S"));
    let n = events(&s).len();
    let (status, answer) = delete(&s, &t, w);
    assert_eq!(status, 200, "{answer}");
    let ev = events_after(&s, n);
    let pos = |id: u64| ev.iter().position(|e| e["id"] == id).unwrap();
    assert_eq!(ev.len(), 4, "{ev:?}");
    assert!(
        pos(b) < pos(sub) && pos(sub) < pos(w) && pos(a) < pos(w),
        "{ev:?}"
    );
    assert_eq!(ev[pos(w)]["type"], "folder.deleted");
    assert!(ev.iter().all(|e| e["origin"] == "app"));
    assert_eq!(answer["path"], "NOTE/Note/W");
    assert_eq!(answer["removed"], json!([]));
    assert_eq!(answer["seqs"], seqs(&ev));
    assert!(!files(&s).join("NOTE/Note/W").exists());
    let store = s.state.store("jordi").unwrap();
    let g = store.lock().unwrap();
    let recycled = store.recycle_index(&g).unwrap();
    let ids: BTreeSet<u64> = recycled.iter().map(|r| r.id).collect();
    assert!(ids.contains(&a) && ids.contains(&b), "{recycled:?}");
}

#[test]
fn a_deleted_note_drops_its_page_records() {
    let s = start_imported();
    let t = writer(&s);
    let id = uploaded(&s, &t, "NOTE/Note/t.note", &test_note());
    assert_eq!(record_paths(&s), ["NOTE/Note/t.note"]);
    let (status, answer) = delete(&s, &t, id);
    assert_eq!(status, 200, "{answer}");
    assert!(record_paths(&s).is_empty());
}

#[test]
fn a_retried_delete_is_404() {
    let s = start_imported();
    let t = writer(&s);
    let id = uploaded(&s, &t, "NOTE/Note/a.txt", b"a");
    assert_eq!(delete(&s, &t, id).0, 200);
    let n = events(&s).len();
    let (status, answer) = delete(&s, &t, id);
    assert_eq!(status, 404, "{answer}");
    assert_eq!(answer, json!({"error": "not_found"}));
    let (status, answer) = mv(&s, &t, id, "NOTE/Note/b.txt");
    assert_eq!(status, 404, "{answer}");
    assert_eq!(answer, json!({"error": "not_found"}));
    assert_eq!(delete(&s, &t, 999_999).0, 404);
    assert_eq!(events(&s).len(), n);
}

#[test]
fn a_delete_of_the_last_file_reports_the_removed_folder() {
    let s = start_imported();
    let t = writer(&s);
    let a = uploaded(&s, &t, "NOTE/Note/P/Q/a.txt", b"a");
    let (p, q) = (id_of(&s, "NOTE/Note/P"), id_of(&s, "NOTE/Note/P/Q"));
    let (status, answer) = delete(&s, &t, a);
    assert_eq!(status, 200, "{answer}");
    assert_eq!(
        answer["removed"],
        json!([{"id": q, "path": "NOTE/Note/P/Q"}, {"id": p, "path": "NOTE/Note/P"}])
    );
    assert!(!files(&s).join("NOTE/Note/P").exists());
}

#[test]
fn a_folder_delete_reports_only_the_folders_it_left_empty() {
    let s = start_imported();
    let t = writer(&s);
    uploaded(&s, &t, "NOTE/Note/P/A/sub/x.txt", b"x");
    let (p, a) = (id_of(&s, "NOTE/Note/P"), id_of(&s, "NOTE/Note/P/A"));
    let (status, answer) = delete(&s, &t, a);
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["removed"], json!([{"id": p, "path": "NOTE/Note/P"}]));
}

#[test]
fn a_bad_write_body_is_400() {
    let s = start_imported();
    let t = writer(&s);
    let a = uploaded(&s, &t, "NOTE/Note/a.txt", b"a");
    for (path, body) in [
        ("/folders", json!({})),
        ("/folders", json!({"path": 1})),
        (
            "/move",
            json!({"id": a.to_string(), "to": "NOTE/Note/b.txt"}),
        ),
        ("/move", json!({"id": a})),
        ("/move", json!({"to": "NOTE/Note/b.txt"})),
    ] {
        let (status, answer, _) = send_json(&s, &t, "POST", path, Some(body.clone()));
        assert_eq!(status, 400, "{path} {body}: {answer}");
        assert_eq!(answer["error"], "bad_request", "{path} {body}");
    }
    let (status, _, _) = send_json(&s, &t, "DELETE", "/entries/abc", None);
    assert_eq!(status, 400);
    assert!(files(&s).join("NOTE/Note/a.txt").exists());
}

/// Every kind of write once, for the gates: (what, status, answer, `Retry-After`).
fn every_write(s: &Server, t: &str, id: u64) -> Vec<(&'static str, u16, Value, Option<String>)> {
    let r = s
        .client
        .put(s.url(&format!("/files?path={}", q("NOTE/Note/new.txt"))))
        .bearer_auth(t)
        .body("new")
        .send()
        .unwrap();
    let retry = header(&r, "retry-after");
    let (status, answer) = json_of(r);
    let mut all = vec![("upload", status, answer, retry)];
    let (status, answer, retry) = send_json(
        s,
        t,
        "POST",
        "/folders",
        Some(json!({"path": "NOTE/Note/New"})),
    );
    all.push(("folder", status, answer, retry));
    let (status, answer, retry) = send_json(
        s,
        t,
        "POST",
        "/move",
        Some(json!({"id": id, "to": "NOTE/Note/moved.txt"})),
    );
    all.push(("move", status, answer, retry));
    let (status, answer, retry) = send_json(s, t, "DELETE", &format!("/entries/{id}"), None);
    all.push(("delete", status, answer, retry));
    all
}

#[test]
fn writes_wait_while_a_scan_holds_the_account() {
    let s = start_imported();
    let t = writer(&s);
    // A folder to move and delete: a file would keep the account above the hold's minimum.
    let (status, folder) = mkdir(&s, &t, "NOTE/Note/F");
    assert_eq!(status, 201, "{folder}");
    let f = folder["id"].as_u64().unwrap();
    let gone = s.put("gone.note", b"g");
    std::fs::remove_file(files(&s).join(&gone)).unwrap();
    assert_eq!(
        s.get(Some(&t), &format!("/tree?path={}", q("NOTE/Note"))).0,
        423
    );
    let n = events(&s).len();
    for (what, status, answer, _) in every_write(&s, &t, f) {
        assert_eq!(status, 423, "{what}: {answer}");
        assert_eq!(answer, json!({"error": "scan_hold"}), "{what}");
    }
    assert_eq!(events(&s).len(), n);
    assert!(files(&s).join("NOTE/Note/F").is_dir());
    assert!(!files(&s).join("NOTE/Note/New").exists());
}

#[test]
fn writes_wait_for_the_import_in_protect_mode() {
    let s = start();
    assert!(s.state.config.protect);
    let t = writer(&s);
    let n = events(&s).len();
    // Any id: the gate comes before the lookup.
    for (what, status, answer, _) in every_write(&s, &t, 1000) {
        assert_eq!(status, 423, "{what}: {answer}");
        assert_eq!(answer, json!({"error": "not_imported"}), "{what}");
    }
    assert_eq!(events(&s).len(), n);
}

#[test]
fn a_write_during_a_device_sync_waits() {
    use crate::fake_device::{SERIAL, sign_in};
    let s = start_imported();
    let t = writer(&s);
    let a = uploaded(&s, &t, "NOTE/Note/a.txt", b"a");
    let base = s.base.clone();
    let mut d = sign_in(base, EMAIL, PASSWORD);
    d.ok(
        "/api/file/2/files/synchronous/start",
        json!({"equipmentNo": SERIAL}),
    );
    let n = events(&s).len();
    for (what, status, answer, retry) in every_write(&s, &t, a) {
        assert_eq!(status, 423, "{what}: {answer}");
        assert_eq!(answer, json!({"error": "device_syncing"}), "{what}");
        assert_eq!(retry.as_deref(), Some("5"), "{what}");
    }
    // Reads always work.
    assert_eq!(
        s.get(Some(&t), &format!("/tree?path={}", q("NOTE/Note"))).0,
        200
    );
    assert_eq!(events(&s).len(), n);
    d.ok(
        "/api/file/2/files/synchronous/end",
        json!({"equipmentNo": SERIAL, "flag": "Y"}),
    );
    let done = every_write(&s, &t, a);
    let codes: Vec<u16> = done.iter().map(|(_, status, _, _)| *status).collect();
    assert_eq!(codes, [201, 201, 200, 200], "{done:?}");
    assert!(files(&s).join("NOTE/Note/new.txt").exists());
    assert!(files(&s).join("NOTE/Note/New").is_dir());
    assert!(!files(&s).join("NOTE/Note/moved.txt").exists());
    assert!(!files(&s).join("NOTE/Note/a.txt").exists());
}

#[test]
fn a_staged_device_upload_survives_an_app_write_of_another_path() {
    use crate::fake_device::{SERIAL, multipart, sign_in};
    let s = start_imported();
    let t = writer(&s);
    let a = uploaded(&s, &t, "NOTE/Note/a.txt", b"a");
    let mut d = sign_in(s.base.clone(), EMAIL, PASSWORD);
    d.ok(
        "/api/file/2/files/synchronous/start",
        json!({"equipmentNo": SERIAL}),
    );
    let bytes = b"device note bytes".to_vec();
    let applied = d.ok(
        "/api/file/3/files/upload/apply",
        json!({"equipmentNo": SERIAL, "path": "/NOTE/Note/dev.note",
               "fileName": "dev.note", "size": bytes.len()}),
    );
    let url = applied["fullUploadUrl"].as_str().unwrap().to_string();
    let (ct, body) = multipart("dev.note", &bytes);
    let req = d.http.post(&url).header("content-type", ct).body(body);
    let (status, _) = d.send(req);
    assert_eq!(status, 200);
    d.ok(
        "/api/file/2/files/synchronous/end",
        json!({"equipmentNo": SERIAL, "flag": "Y"}),
    );
    // The app writes other paths while the device's bytes wait in staging.
    let codes: Vec<u16> = every_write(&s, &t, a)
        .iter()
        .map(|(_, status, _, _)| *status)
        .collect();
    assert_eq!(codes, [201, 201, 200, 200]);
    d.ok(
        "/api/file/2/files/synchronous/start",
        json!({"equipmentNo": SERIAL}),
    );
    d.ok(
        "/api/file/2/files/upload/finish",
        json!({"equipmentNo": SERIAL, "path": "/NOTE/Note/", "fileName": "dev.note",
               "content_hash": md5_hex(&bytes), "innerName": applied["innerName"],
               "size": bytes.len().to_string()}),
    );
    d.ok(
        "/api/file/2/files/synchronous/end",
        json!({"equipmentNo": SERIAL, "flag": "Y"}),
    );
    assert_eq!(
        std::fs::read(files(&s).join("NOTE/Note/dev.note")).unwrap(),
        bytes
    );
}

#[test]
fn a_read_token_cannot_write() {
    let s = start_imported();
    let w = writer(&s);
    let a = uploaded(&s, &w, "NOTE/Note/a.txt", b"a");
    let t = s.token();
    for (what, status, answer, _) in every_write(&s, &t, a) {
        assert_eq!(status, 403, "{what}: {answer}");
        assert_eq!(answer, json!({"error": "read_only_token"}), "{what}");
    }
    assert!(files(&s).join("NOTE/Note/a.txt").exists());
}

#[test]
fn a_removed_account_token_cannot_write() {
    let s = start_imported();
    let t = writer(&s);
    let a = uploaded(&s, &t, "NOTE/Note/a.txt", b"a");
    let accounts = s.state.auth.accounts();
    accounts.remove("jordi", &s.state.journal).unwrap();
    accounts.create("jordi", EMAIL, PASSWORD).unwrap();
    for (what, status, answer, _) in every_write(&s, &t, a) {
        assert_eq!(status, 410, "{what}: {answer}");
        assert_eq!(answer["error"], "account_removed", "{what}");
    }
}

#[test]
fn a_refused_token_cannot_write() {
    let s = start_imported();
    let t = writer(&s);
    let a = uploaded(&s, &t, "NOTE/Note/a.txt", b"a");
    for (what, status, answer, _) in every_write(&s, "nonsense", a) {
        assert_eq!(status, 401, "{what}: {answer}");
        assert_eq!(answer["error"], "token_refused", "{what}");
    }
    assert!(files(&s).join("NOTE/Note/a.txt").exists());
}

#[test]
fn a_large_json_body_is_answered_in_json() {
    let s = start_imported();
    let t = writer(&s);
    let post = |mib: usize| {
        let body = format!(
            "{{\"path\":\"NOTE/Note/{}\"}}",
            "a".repeat(mib * 1024 * 1024)
        );
        json_of(
            s.client
                .post(s.url("/folders"))
                .bearer_auth(&t)
                .header("content-type", "application/json")
                .body(body)
                .send()
                .unwrap(),
        )
    };
    // Between axum's default 2 MiB and the 8 MiB limit: the handler reads it and refuses the
    // name, in JSON (it was axum's plain text before).
    let (status, answer) = post(3);
    assert_eq!(
        (status, &answer),
        (400, &json!({"error": "name_too_long"})),
        "{answer}"
    );
    // Over 8 MiB: the JSON 413 of the request log.
    let (status, answer) = post(9);
    assert_eq!(status, 413, "{answer}");
    assert_eq!(answer, json!({"error": "too_large"}));
}
