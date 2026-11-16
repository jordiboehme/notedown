//! `GET /rest/v1/events`: the file and folder events of the token's account, on a real socket,
//! as the app follows them after a listing.

use std::collections::BTreeMap;

use notedown_cloud::auth::{Accounts, revoke_token};
use notedown_cloud::journal::{Event, EventBody, FileKind, Origin, new_op_id};
use notedown_cloud::read;
use serde_json::{Value, json};

use crate::rest_server::{
    EMAIL, PASSWORD, Server, delete, mkdir, mv, start, start_imported, upload, uploaded, writer,
};

/// One page of `/events` from `from` (200).
fn page(s: &Server, token: &str, from: u64) -> Value {
    let (status, page) = s.get(Some(token), &format!("/events?from={from}"));
    assert_eq!(status, 200, "{page}");
    page
}

/// Every event from `from` on, page by page, and the last page.
fn follow(s: &Server, token: &str, from: u64) -> (Vec<Value>, Value) {
    let mut from = from;
    let mut all = Vec::new();
    loop {
        let p = page(s, token, from);
        all.extend(p["events"].as_array().unwrap().iter().cloned());
        let next = p["next"].as_u64().unwrap();
        if !p["more"].as_bool().unwrap() {
            return (all, p);
        }
        assert!(next > from, "{p}");
        from = next;
    }
}

fn types(events: &[Value]) -> Vec<&str> {
    events.iter().map(|e| e["type"].as_str().unwrap()).collect()
}

/// The events whose `path` is `path`.
fn at<'a>(events: &'a [Value], path: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["path"] == path).collect()
}

/// The seq of the `account.removed` of `user_id` in the journal.
fn removal_of(s: &Server, user_id: u64) -> u64 {
    read::journal(&s.data, "jordi", 0, 10_000)
        .unwrap()
        .removed
        .iter()
        .find(|r| r.user_id == user_id)
        .unwrap()
        .seq
}

/// Removes the account `jordi` and creates it again; returns the old and the new user id.
fn remove_and_create(s: &Server) -> (u64, u64) {
    let accounts = s.state.auth.accounts();
    let old = accounts.get("jordi").unwrap().unwrap().user_id;
    accounts.remove("jordi", &s.state.journal).unwrap();
    let new = accounts.create("jordi", EMAIL, PASSWORD).unwrap().user_id;
    assert_ne!(old, new);
    (old, new)
}

/// `GET /tree?recursive=true` of the root: (seq, entries).
fn listing(s: &Server, token: &str) -> (u64, Vec<Value>) {
    let (status, tree) = s.get(Some(token), "/tree?recursive=true");
    assert_eq!(status, 200, "{tree}");
    (
        tree["seq"].as_u64().unwrap(),
        tree["entries"].as_array().unwrap().clone(),
    )
}

#[test]
fn events_follow_uploads_moves_and_deletes() {
    let s = start_imported();
    let t = writer(&s);
    let (seq, _) = listing(&s, &t);
    let (status, created) = upload(&s, &t, "NOTE/Note/a.txt", b"hello");
    assert_eq!(status, 201, "{created}");
    let id = created["id"].as_u64().unwrap();
    let (status, moved) = mv(&s, &t, id, "NOTE/Note/b.txt");
    assert_eq!(status, 200, "{moved}");
    let (status, deleted) = delete(&s, &t, id);
    assert_eq!(status, 200, "{deleted}");

    let p = page(&s, &t, seq + 1);
    let events = p["events"].as_array().unwrap().clone();
    assert_eq!(
        types(&events),
        ["file.created", "file.moved", "file.deleted"],
        "{p}"
    );
    let e = &events[0];
    assert_eq!(e["id"], id);
    assert_eq!(e["path"], "NOTE/Note/a.txt");
    assert_eq!(e["md5"], created["md5"]);
    assert_eq!(e["size"], 5);
    assert_eq!(e["fileKind"], "document");
    assert!(e["ts"].as_u64().unwrap() > 0, "{e}");
    assert!(e["opId"].as_str().is_some_and(|o| !o.is_empty()), "{e}");
    assert!(e.get("kind").is_none(), "{e}");
    assert_eq!(events[1]["oldPath"], "NOTE/Note/a.txt");
    assert_eq!(events[1]["path"], "NOTE/Note/b.txt");
    assert_eq!(events[2]["path"], "NOTE/Note/b.txt");
    for e in &events {
        assert_eq!(e["origin"], "app", "{e}");
        assert_eq!(e["id"], id, "{e}");
    }
    // The seqs of the write answers are the seqs of the events.
    let mut seqs: Vec<Value> = Vec::new();
    for answer in [&created, &moved, &deleted] {
        seqs.extend(answer["seqs"].as_array().unwrap().iter().cloned());
    }
    let got: Vec<Value> = events.iter().map(|e| e["seq"].clone()).collect();
    assert_eq!(got, seqs);
    let last = events[2]["seq"].as_u64().unwrap();
    assert_eq!(p["next"], last + 1);
    assert_eq!(p["end"], last);
    assert_eq!(
        (p["more"].clone(), p["gap"].clone()),
        (json!(false), json!(false))
    );

    // `limit` is at least 1; a larger one than allowed is taken as the largest.
    let (status, one) = s.get(Some(&t), &format!("/events?from={}&limit=0", seq + 1));
    assert_eq!(status, 200, "{one}");
    assert_eq!(one["events"].as_array().unwrap().len(), 1);
    assert_eq!(one["more"], true);
    let (status, all) = s.get(
        Some(&t),
        &format!("/events?from={}&limit=99999999", seq + 1),
    );
    assert_eq!((status, all), (200, p));
    for bad in [
        "/events?from=x",
        "/events?from=0&limit=-1",
        "/events?limit=y",
    ] {
        let (status, body) = s.get(Some(&t), bad);
        assert_eq!(
            (status, &body["error"]),
            (400, &json!("bad_request")),
            "{bad}"
        );
    }
}

#[test]
fn events_come_in_pages_of_at_most_1000() {
    let s = start();
    let token = s.token();
    let mut many: Vec<Event> = (0..1001)
        .map(|i| {
            Event::new(
                "jordi",
                Origin::Cli,
                None,
                &new_op_id(),
                EventBody::FileCreated {
                    id: 1_000 + i,
                    path: format!("NOTE/Note/n{i}.txt"),
                    kind: FileKind::Document,
                    md5: "0".repeat(32),
                    size: 1,
                    pages_changed: None,
                },
            )
        })
        .collect();
    s.state.journal.append(&mut many).unwrap();
    for path in ["/events", "/events?from=0&limit=1001", "/events?limit=5000"] {
        let (status, p) = s.get(Some(&token), path);
        assert_eq!(status, 200, "{p}");
        assert_eq!(p["events"].as_array().unwrap().len(), 1000, "{path}");
        assert_eq!(p["more"], true, "{path}");
    }
    let (events, last) = follow(&s, &token, 0);
    assert_eq!(events.len(), 1001);
    assert_eq!(last["gap"], false);
}

#[test]
fn events_include_cli_changes() {
    let s = start();
    let token = s.token();
    let path = s.put("a.note", b"a");
    let p = page(&s, &token, 0);
    let events = p["events"].as_array().unwrap();
    let created = at(events, &path);
    assert_eq!(created.len(), 1, "{p}");
    assert_eq!(created[0]["type"], "file.created");
    assert_eq!(created[0]["origin"], "cli");
    assert_eq!(created[0]["fileKind"], "note");
    assert_eq!(created[0]["size"], 1);
    assert_eq!(p["gap"], false);
}

#[test]
fn events_after_a_listing_seq_have_no_gap() {
    let s = start_imported();
    let t = writer(&s);
    uploaded(&s, &t, "NOTE/Note/a.txt", b"a");
    let (seq, entries) = listing(&s, &t);
    assert!(entries.iter().any(|e| e["path"] == "NOTE/Note/a.txt"));
    let b = uploaded(&s, &t, "NOTE/Note/b.txt", b"b");
    let p = page(&s, &t, seq + 1);
    let events = p["events"].as_array().unwrap();
    assert_eq!(types(events), ["file.created"], "{p}");
    assert_eq!(events[0]["id"], b);
    assert_eq!(p["gap"], false);
    // A quiet poll from `next` is empty and no gap either.
    let quiet = page(&s, &t, p["next"].as_u64().unwrap());
    assert_eq!(quiet["events"], json!([]));
    assert_eq!(quiet["gap"], false);
    assert_eq!(quiet["next"], p["next"]);
}

#[test]
fn a_new_account_sees_nothing_of_the_old_one_with_its_slug() {
    let s = start();
    let old_token = s.token();
    let a = s.put("a.note", b"a");
    let (old, new) = remove_and_create(&s);
    let b = s.put("b.note", b"b");
    let removal = removal_of(&s, old);
    let new_token = s.token();

    let (events, last) = follow(&s, &new_token, 0);
    assert_eq!(
        last["gap"], true,
        "from 0 is below the earlier account's removal"
    );
    assert!(
        events.iter().all(|e| e["seq"].as_u64().unwrap() > removal),
        "{events:?}"
    );
    assert!(at(&events, &a).is_empty(), "{events:?}");
    assert_eq!(at(&events, &b).len(), 1, "{events:?}");
    assert!(!types(&events).contains(&"account.removed"), "{events:?}");
    // From the removal on, as the app goes on after its listing, there is no gap.
    let p = page(&s, &new_token, removal + 1);
    assert_eq!(p["gap"], false, "{p}");

    // The old token gets only the old account, up to its removal.
    let (events, _) = follow(&s, &old_token, 0);
    assert_eq!(at(&events, &a).len(), 1, "{events:?}");
    assert!(at(&events, &b).is_empty(), "{events:?}");
    let end = events.last().unwrap();
    assert_eq!(end["type"], "account.removed");
    assert_eq!(end["seq"], removal);
    assert_eq!(end["userId"], old);
    assert_ne!(old, new);
}

#[test]
fn a_removed_account_token_ends_with_its_removal() {
    let s = start();
    let token = s.token();
    let a = s.put("a.note", b"a");
    let (old, _) = remove_and_create(&s);
    s.put("b.note", b"b");
    let until = removal_of(&s, old);

    let p = page(&s, &token, 0);
    let events = p["events"].as_array().unwrap();
    assert_eq!(at(events, &a).len(), 1, "{p}");
    let end = events.last().unwrap();
    assert_eq!(end["type"], "account.removed");
    assert_eq!(end["seq"], until);
    assert_eq!(end["userId"], old);
    assert!(
        end.get("id").is_none() && end.get("path").is_none(),
        "{end}"
    );
    assert_eq!(
        types(events)
            .iter()
            .filter(|t| **t == "account.removed")
            .count(),
        1
    );
    assert_eq!(p["next"], until + 1);
    assert_eq!(p["more"], false);
    assert_eq!(p["gap"], false);
    // A poll past the removal stays there and is no gap.
    let after = page(&s, &token, until + 1);
    assert_eq!(after["events"], json!([]), "{after}");
    assert_eq!(after["gap"], false, "{after}");
    assert_eq!(after["more"], false, "{after}");
}

#[test]
fn an_old_token_of_a_reused_slug_ends_with_its_own_removal() {
    let s = start();
    let first_token = s.token();
    let a = s.put("a.note", b"a");
    let (first, _) = remove_and_create(&s);
    let second_token = s.token();
    let b = s.put("b.note", b"b");
    let (second, _) = remove_and_create(&s);
    let third_token = s.token();
    let c = s.put("c.note", b"c");
    let (r1, r2) = (removal_of(&s, first), removal_of(&s, second));
    assert!(r1 < r2);

    // The first token is used for the first time after the second account was removed too.
    let (events, last) = follow(&s, &first_token, 0);
    assert_eq!(at(&events, &a).len(), 1, "{events:?}");
    assert!(at(&events, &b).is_empty() && at(&events, &c).is_empty());
    assert_eq!(events.last().unwrap()["seq"], r1);
    assert_eq!(events.last().unwrap()["type"], "account.removed");
    assert_eq!(last["gap"], false);

    let (events, last) = follow(&s, &second_token, 0);
    assert_eq!(at(&events, &b).len(), 1, "{events:?}");
    assert!(at(&events, &a).is_empty() && at(&events, &c).is_empty());
    assert!(events.iter().all(|e| e["seq"].as_u64().unwrap() > r1));
    assert_eq!(events.last().unwrap()["seq"], r2);
    assert_eq!(events.last().unwrap()["userId"], second);
    assert_eq!(last["gap"], true, "from 0 is below the first removal");

    let (events, _) = follow(&s, &third_token, 0);
    assert_eq!(at(&events, &c).len(), 1, "{events:?}");
    assert!(at(&events, &a).is_empty() && at(&events, &b).is_empty());
    assert!(events.iter().all(|e| e["seq"].as_u64().unwrap() > r2));
}

#[test]
fn the_own_removal_record_of_a_live_account_is_no_floor() {
    let s = start();
    let token = s.token();
    let a = s.put("a.note", b"a");
    // A crash between the record and the removal leaves the live account with its record.
    let user_id = Accounts::new(&s.data)
        .get("jordi")
        .unwrap()
        .unwrap()
        .user_id;
    let mut record = [Event::new(
        "jordi",
        Origin::Cli,
        None,
        &new_op_id(),
        EventBody::AccountRemoved { user_id },
    )];
    s.state.journal.append(&mut record).unwrap();
    let b = s.put("b.note", b"b");
    let (events, last) = follow(&s, &token, 0);
    assert_eq!(at(&events, &a).len(), 1, "{events:?}");
    assert_eq!(at(&events, &b).len(), 1, "{events:?}");
    assert!(!types(&events).contains(&"account.removed"));
    assert_eq!(last["gap"], false);
}

/// What the app keeps of an entry: its path, its kind and, for a file, md5 and size.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Kept {
    path: String,
    kind: String,
    md5: Option<String>,
    size: Option<u64>,
}

fn view_of(entries: &[Value]) -> BTreeMap<u64, Kept> {
    entries
        .iter()
        .map(|e| {
            let file = e["kind"] == "file";
            (
                e["id"].as_u64().unwrap(),
                Kept {
                    path: e["path"].as_str().unwrap().to_string(),
                    kind: e["kind"].as_str().unwrap().to_string(),
                    md5: file.then(|| e["md5"].as_str().unwrap().to_string()),
                    size: file.then(|| e["size"].as_u64().unwrap()),
                },
            )
        })
        .collect()
}

fn below(path: &str, folder: &str) -> bool {
    path.strip_prefix(folder)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Applies one event with the documented rules. An id the view does not hold in
/// `file.modified`, `file.moved` or as a copy source panics on purpose: in this fixed scenario
/// every such id was listed first, so an unknown one is a bug (an app asks `GET /entries/{id}`).
fn apply(view: &mut BTreeMap<u64, Kept>, e: &Value) {
    let id = e["id"].as_u64();
    let path = e["path"].as_str().map(str::to_string);
    let text = |k: &str| e[k].as_str().map(str::to_string);
    match e["type"].as_str().unwrap() {
        t @ ("file.created" | "folder.created") => {
            let (id, path) = (id.unwrap(), path.unwrap());
            // A create at a path held by another id replaces that entry.
            view.retain(|other, k| *other == id || k.path != path);
            let file = t == "file.created";
            view.insert(
                id,
                Kept {
                    path,
                    kind: if file { "file" } else { "folder" }.into(),
                    md5: text("md5"),
                    size: e["size"].as_u64(),
                },
            );
        }
        "file.modified" => {
            let k = view.get_mut(&id.unwrap()).unwrap();
            k.md5 = text("md5");
            k.size = e["size"].as_u64();
        }
        "file.moved" => view.get_mut(&id.unwrap()).unwrap().path = path.unwrap(),
        "folder.moved" => {
            let (old, new) = (text("oldPath").unwrap(), path.unwrap());
            for k in view.values_mut() {
                if k.path == old {
                    k.path = new.clone();
                } else if below(&k.path, &old) {
                    k.path = format!("{new}{}", &k.path[old.len()..]);
                }
            }
        }
        "file.copied" => {
            let mut k = view[&e["sourceId"].as_u64().unwrap()].clone();
            k.path = path.unwrap();
            view.insert(id.unwrap(), k);
        }
        "file.deleted" => {
            view.remove(&id.unwrap());
        }
        "folder.deleted" => {
            let path = path.unwrap();
            view.retain(|other, k| *other != id.unwrap() && !below(&k.path, &path));
        }
        other => panic!("unexpected event {other}: {e}"),
    }
}

#[test]
fn replaying_events_gives_the_new_tree() {
    let s = start_imported();
    let t = writer(&s);
    let (status, folder) = mkdir(&s, &t, "NOTE/Note/F");
    assert_eq!(status, 201, "{folder}");
    let f = folder["id"].as_u64().unwrap();
    uploaded(&s, &t, "NOTE/Note/F/a.txt", b"a");
    uploaded(&s, &t, "NOTE/Note/F/sub/b.txt", b"bb");
    uploaded(&s, &t, "NOTE/Note/F/sub/deep/c.txt", b"ccc");
    uploaded(&s, &t, "NOTE/Note/keep.txt", b"keep");
    s.put("cli.note", b"cli");
    let (seq, entries) = listing(&s, &t);
    let mut view = view_of(&entries);

    let (status, answer) = mv(&s, &t, f, "NOTE/Note/G");
    assert_eq!(status, 200, "{answer}");
    uploaded(&s, &t, "NOTE/Note/G/sub/d.txt", b"dddd");
    let (status, answer) = upload(&s, &t, "NOTE/Note/G/a.txt", b"changed");
    assert_eq!(status, 200, "{answer}");
    let sub = view
        .iter()
        .find(|(_, k)| k.path == "NOTE/Note/F/sub")
        .map(|(id, _)| *id)
        .unwrap();
    let (status, answer) = delete(&s, &t, sub);
    assert_eq!(status, 200, "{answer}");

    let (events, last) = follow(&s, &t, seq + 1);
    assert_eq!(last["gap"], false);
    assert!(types(&events).contains(&"folder.moved"), "{events:?}");
    assert!(types(&events).contains(&"folder.deleted"), "{events:?}");
    for e in &events {
        apply(&mut view, e);
    }
    let (_, entries) = listing(&s, &t);
    assert_eq!(view, view_of(&entries));
}

#[test]
fn a_read_token_can_follow_events() {
    let s = start();
    let (status, login) = s.login_extra(json!({}));
    assert_eq!(status, 201, "{login}");
    assert_eq!(login["scope"], "read");
    let token = login["token"].as_str().unwrap();
    let path = s.put("a.note", b"a");
    let p = page(&s, token, 0);
    assert_eq!(at(p["events"].as_array().unwrap(), &path).len(), 1, "{p}");
}

#[test]
fn a_refused_token_cannot() {
    let s = start();
    let (_, answer, _) = s.login_with(EMAIL, PASSWORD);
    let token = answer["token"].as_str().unwrap();
    assert_eq!(s.get(Some(token), "/events").0, 200);
    revoke_token(&s.data, "jordi", answer["id"].as_str().unwrap()).unwrap();
    let (status, body) = s.get(Some(token), "/events?from=0");
    assert_eq!((status, body), (401, json!({"error": "token_refused"})));
    let (status, body) = s.get(None, "/events");
    assert_eq!((status, body), (401, json!({"error": "token_refused"})));
    let (status, _) = s.get(Some("ndt1.e30.AAAA"), "/events");
    assert_eq!(status, 401);
}

#[test]
fn quiet_event_polls_are_not_logged() {
    let s = start();
    let token = s.token();
    s.put("a.note", b"a");
    let next = page(&s, &token, 0)["next"].as_u64().unwrap();
    let before = s.log_lines().len();
    for _ in 0..10 {
        assert_eq!(s.get(Some(&token), &format!("/events?from={next}")).0, 200);
    }
    assert_eq!(s.get(Some(&token), "/events?from=0&limit=1").0, 200);
    assert_eq!(
        s.log_lines().len(),
        before,
        "200 answers to polls add no line"
    );
    // A bad request is logged.
    assert_eq!(s.get(Some(&token), "/events?from=x").0, 400);
    assert_eq!(s.log_lines().len(), before + 1);
}

/// Copies the folder `from` with everything in it to `to`.
fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let target = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &target);
        } else {
            std::fs::copy(e.path(), target).unwrap();
        }
    }
}

#[test]
fn an_account_back_from_a_backup_sees_nothing_of_a_later_one() {
    let s = start();
    let token = s.token();
    s.put("a.note", b"a");
    // The first poll of the live account.
    assert_eq!(page(&s, &token, 0)["gap"], false);
    let dir = s.data.join("accounts/jordi");
    let backup = s.data.parent().unwrap().join("backup");
    copy_dir(&dir, &backup);
    let (first, _) = remove_and_create(&s);
    let b = s.put("b.note", b"b");
    let accounts = s.state.auth.accounts();
    let second = accounts.get("jordi").unwrap().unwrap().user_id;
    accounts.remove("jordi", &s.state.journal).unwrap();
    // The first account comes back from its backup, with its token.
    copy_dir(&backup, &dir);
    assert_eq!(
        accounts.get("jordi").unwrap().unwrap().user_id,
        first,
        "restored"
    );
    let r2 = removal_of(&s, second);
    let (events, last) = follow(&s, &token, 0);
    assert!(at(&events, &b).is_empty(), "{events:?}");
    assert!(
        events.iter().all(|e| e["seq"].as_u64().unwrap() > r2),
        "{events:?}"
    );
    assert_eq!(last["gap"], true);
}
