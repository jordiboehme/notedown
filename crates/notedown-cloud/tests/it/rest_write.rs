//! The REST routes of the Mac app: browse (`/tree`, `/entries/{id}`) and download
//! (`/files/{id}`), on a real socket next to the device routes.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::PathBuf;

use md5::{Digest, Md5};
use notedown_cloud::auth::Accounts;
use notedown_cloud::put;
use notedown_cloud::store::WirePath;
use serde_json::{Value, json};

use crate::rest_server::{EMAIL, PASSWORD, Server, json_of, q, start, start_imported};

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

fn header(r: &reqwest::blocking::Response, name: &str) -> Option<String> {
    r.headers()
        .get(name)
        .map(|v| v.to_str().unwrap().to_string())
}

#[test]
fn tree_lists_the_children_of_the_root() {
    let s = start_imported();
    let token = s.token();
    let (status, answer) = s.get(Some(&token), "/tree");
    assert_eq!(status, 200, "{answer}");
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
    let (status, answer) = s.get(Some(&token), "/tree?recursive=true");
    assert_eq!(status, 200);
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
