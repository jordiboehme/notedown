//! Changes and the recycle bin against a scripted server on loopback: the requests are answered
//! in order, and a wait for a syncing Supernote moves a fake clock instead of sleeping.

use serde_json::{Value, json};

use super::listing_tests::{
    MD5_A, NOTE, Setup, entry, ev, events, file, folder, gap, setup, setup_hooked, tree,
};
use super::*;
use crate::rest::test_http::{answer, answer_with};

const KEY: &str = "1763290000000/NOTE/Note/b.note";

fn ok(body: Value) -> String {
    answer("200 OK", &body.to_string())
}

fn created(body: Value) -> String {
    answer("201 Created", &body.to_string())
}

fn error(status: &str, body: Value) -> String {
    answer(status, &body.to_string())
}

fn syncing(retry_after: &str) -> String {
    answer_with(
        "423 Locked",
        &[("retry-after", retry_after)],
        r#"{"error":"device_syncing"}"#,
    )
}

/// The events sent since the last call.
fn take(s: &Setup) -> Vec<LibraryEvent> {
    std::mem::take(&mut *s.events.0.lock().unwrap())
}

/// The job events sent since the last call (the other events are dropped).
fn jobs(s: &Setup) -> Vec<Job> {
    take(s)
        .into_iter()
        .filter_map(|e| match e {
            LibraryEvent::Job { job } => Some(job),
            _ => None,
        })
        .collect()
}

/// How many `recycle` events came since the last call (the other events are dropped).
fn recycles(s: &Setup) -> usize {
    take(s)
        .into_iter()
        .filter(|e| *e == LibraryEvent::Recycle)
        .count()
}

fn states(jobs: &[Job]) -> Vec<JobState> {
    jobs.iter().map(|j| j.state).collect()
}

fn done(id: u64, label: &str, result: Option<&str>) -> Job {
    Job {
        id,
        kind: JobKind::Change,
        label: label.to_owned(),
        done: 0,
        total: None,
        state: JobState::Done,
        text: None,
        question: None,
        result: result.map(str::to_owned),
        drop_id: None,
    }
}

fn failed(job: &Job, text: &str) {
    assert_eq!(job.kind, JobKind::Change);
    assert_eq!(job.state, JobState::Failed, "{job:?}");
    assert_eq!(job.text.as_deref(), Some(text));
    assert_eq!(job.question, None);
}

/// The next request the server got: method, target without `/rest/v1`, and the body.
fn request(s: &Setup) -> (String, String, Vec<u8>) {
    let r = s.server.next();
    (
        r.method,
        r.target.trim_start_matches("/rest/v1").to_owned(),
        r.body,
    )
}

fn json_body(body: &[u8]) -> Value {
    serde_json::from_slice(body).expect("the body is no JSON")
}

#[test]
fn new_folder_rename_move_delete_call_the_routes() {
    let s = setup(vec![
        created(json!({"id": 7, "path": "NOTE/Note/projects", "seqs": [20]})),
        entry(&file(5, "NOTE/Note/a.note", MD5_A)),
        ok(json!({"id": 5, "path": "NOTE/Note/b.note", "seqs": [21], "removed": []})),
        entry(&file(5, "NOTE/Note/b.note", MD5_A)),
        ok(
            json!({"id": 5, "path": "NOTE/Note/projects/b.note", "seqs": [22],
                   "removed": [{"id": 9, "path": "NOTE/Note/old"}]}),
        ),
        entry(&folder(7, "NOTE/Note/projects")),
        ok(json!({"id": 7, "path": "NOTE/Note/projects", "seqs": [23, 24], "removed": []})),
        created(json!({"id": 8, "path": "EXPORT/2026", "seqs": [25, 26]})),
    ]);
    take(&s);

    let job = s.library.new_folder(NOTE, "projects");
    assert_eq!(
        job,
        done(1, "New folder projects", Some("NOTE/Note/projects"))
    );
    let (method, target, body) = request(&s);
    assert_eq!((method.as_str(), target.as_str()), ("POST", "/folders"));
    assert_eq!(json_body(&body), json!({"path": "NOTE/Note/projects"}));
    let sent = jobs(&s);
    assert_eq!(states(&sent), [JobState::Running, JobState::Done]);
    assert_eq!(sent[1], job);

    let job = s.library.rename(5, "b.note");
    assert_eq!(
        job,
        done(2, "Rename a.note to b.note", Some("NOTE/Note/b.note"))
    );
    assert_eq!(request(&s), ("GET".into(), "/entries/5".into(), Vec::new()));
    let (method, target, body) = request(&s);
    assert_eq!((method.as_str(), target.as_str()), ("POST", "/move"));
    assert_eq!(json_body(&body), json!({"id": 5, "to": "NOTE/Note/b.note"}));
    assert_eq!(jobs(&s).last(), Some(&job));

    let job = s.library.move_to(5, "NOTE/Note/projects");
    assert_eq!(
        job,
        done(
            3,
            "Move b.note to projects",
            Some("NOTE/Note/projects/b.note")
        )
    );
    assert_eq!(request(&s).1, "/entries/5");
    let (method, target, body) = request(&s);
    assert_eq!((method.as_str(), target.as_str()), ("POST", "/move"));
    assert_eq!(
        json_body(&body),
        json!({"id": 5, "to": "NOTE/Note/projects/b.note"})
    );
    jobs(&s);

    let job = s.library.delete(7);
    assert_eq!(job, done(4, "Delete projects", None));
    assert_eq!(request(&s).1, "/entries/7");
    let (method, target, body) = request(&s);
    assert_eq!((method.as_str(), target.as_str()), ("DELETE", "/entries/7"));
    assert!(body.is_empty());
    jobs(&s);

    // A new folder at the top of a tree, and missing folders on the way are the server's.
    let job = s.library.new_folder("EXPORT", "2026");
    assert_eq!(job, done(5, "New folder 2026", Some("EXPORT/2026")));
    assert_eq!(json_body(&request(&s).2), json!({"path": "EXPORT/2026"}));

    // The answers' seqs are the app's own: the follower reads them at once.
    let own: Vec<u64> = lock(&s.library.inner.follow).own.iter().copied().collect();
    assert_eq!(own, [20, 21, 22, 23, 24, 25, 26]);
}

#[test]
fn names_are_sent_in_nfc() {
    // The parent is kept as the server spells it (here NFD); only the typed name becomes NFC.
    let parent = "NOTE/Note/Cafe\u{301}s";
    let s = setup(vec![
        created(json!({"id": 7, "path": "x", "seqs": [20]})),
        entry(&file(5, &format!("{parent}/a.note"), MD5_A)),
        ok(json!({"id": 5, "path": "x", "seqs": [21], "removed": []})),
        ok(json!({"id": 52, "path": "x", "kind": "file", "seqs": [22]})),
    ]);

    s.library.new_folder(parent, "Re\u{301}sume\u{301}");
    assert_eq!(
        json_body(&request(&s).2),
        json!({"path": format!("{parent}/R\u{e9}sum\u{e9}")})
    );

    s.library.rename(5, "Ne\u{301}e.note");
    request(&s);
    assert_eq!(
        json_body(&request(&s).2),
        json!({"id": 5, "to": format!("{parent}/N\u{e9}e.note")})
    );

    // An item of that folder, restored under a new name in it.
    let key = format!("1763290000000/{parent}/b.note");
    s.library
        .restore(&key, Some(&format!("{parent}/b 2 e\u{301}.note")));
    assert_eq!(
        json_body(&request(&s).2),
        json!({"key": key, "path": format!("{parent}/b 2 \u{e9}.note")})
    );
}

#[test]
fn each_error_code_has_its_text() {
    let s = setup(vec![
        error("403 Forbidden", json!({"error": "not_writable"})),
        error(
            "400 Bad Request",
            json!({"error": "bad_name", "message": "x"}),
        ),
        error("400 Bad Request", json!({"error": "name_too_long"})),
        error(
            "409 Conflict",
            json!({"error": "name_taken", "existing": "Projects"}),
        ),
        error(
            "507 Insufficient Storage",
            json!({"error": "quota_exceeded"}),
        ),
        error("423 Locked", json!({"error": "scan_hold"})),
        error("423 Locked", json!({"error": "not_imported"})),
        // A new folder that is there already.
        ok(json!({"id": 7, "path": "NOTE/Note/Projects", "seqs": []})),
        // A Supernote that syncs: waited for by Retry-After, up to two minutes.
        syncing("60"),
        syncing("60"),
        syncing("60"),
        // One that ends while the job waits.
        syncing("5"),
        created(json!({"id": 7, "path": "NOTE/Note/projects", "seqs": [30]})),
        // A delete of something that is gone.
        error("404 Not Found", json!({"error": "not_found"})),
        // A move to the top level.
        entry(&file(5, "NOTE/Note/b.note", MD5_A)),
        error("403 Forbidden", json!({"error": "not_writable"})),
    ]);
    take(&s);
    for text in [
        "This folder cannot be changed.",
        "This name is not allowed.",
        "This name is not allowed.",
        "There is already Projects here.",
        "The account is full.",
        "The server is reading the files of the Supernote. Try again in a moment.",
        "The Supernote has not been imported yet.",
        "There is already Projects here.",
    ] {
        failed(&s.library.new_folder(NOTE, "projects"), text);
    }
    assert_eq!(s.server.received().len(), 8);
    jobs(&s);

    let start = (s.time.clock())();
    let job = s.library.new_folder(NOTE, "projects");
    failed(&job, "The Supernote is syncing. Try again in a moment.");
    assert_eq!((s.time.clock())() - start, 120_000);
    assert_eq!(
        states(&jobs(&s)),
        [
            JobState::Running,
            JobState::Waiting,
            JobState::Running,
            JobState::Waiting,
            JobState::Running,
            JobState::Failed,
        ]
    );

    let job = s.library.new_folder(NOTE, "projects");
    assert_eq!(job.state, JobState::Done);
    let sent = jobs(&s);
    assert_eq!(
        states(&sent),
        [
            JobState::Running,
            JobState::Waiting,
            JobState::Running,
            JobState::Done
        ]
    );
    assert_eq!(
        sent[1].text.as_deref(),
        Some("The Supernote is syncing. Notedown waits.")
    );

    failed(&s.library.delete(3), "This is not there anymore.");
    assert_eq!(s.server.received().len(), 6);

    // Names that are no single name are refused before any request.
    for name in ["", "a/b", ".hidden", ".", "..", "a\0b", &"x".repeat(256)] {
        failed(
            &s.library.new_folder(NOTE, name),
            "This name is not allowed.",
        );
        failed(&s.library.rename(5, name), "This name is not allowed.");
        // In a restore path a `/` divides the names.
        if !name.contains('/') {
            failed(
                &s.library.restore(KEY, Some(&format!("{NOTE}/{name}"))),
                "This name is not allowed.",
            );
        }
    }
    assert!(s.server.received().is_empty());

    // Labels without a name in them.
    assert_eq!(s.library.new_folder(NOTE, "").label, "New folder");
    assert_eq!(s.library.rename(5, "").label, "Rename");
    let job = s.library.move_to(5, "");
    failed(&job, "This folder cannot be changed.");
    assert_eq!(job.label, "Move b.note to the top level");
}

#[test]
fn a_new_binding_ends_a_waiting_change() {
    let s = setup_hooked(vec![syncing("5")], |n, _, library| {
        if n == 0 {
            library.relogin();
        }
    });
    take(&s);
    let start = (s.time.clock())();
    let job = s.library.new_folder(NOTE, "projects");
    assert_eq!(job.state, JobState::Cancelled);
    assert_eq!(job.text, None);
    assert_eq!(
        states(&jobs(&s)),
        [JobState::Running, JobState::Waiting, JobState::Cancelled]
    );
    assert_eq!(s.server.received().len(), 1);
    // The wait ended at once, not after Retry-After.
    assert!((s.time.clock())() - start < 5_000);
}

#[test]
fn a_write_answered_after_a_new_binding_is_not_the_apps_own() {
    let s = setup_hooked(
        vec![created(
            json!({"id": 7, "path": "NOTE/Note/projects", "seqs": [20]}),
        )],
        |n, _, library| {
            if n == 0 {
                library.relogin();
            }
        },
    );
    let job = s.library.new_folder(NOTE, "projects");
    assert_eq!(job.state, JobState::Done);
    assert!(lock(&s.library.inner.follow).own.is_empty());
}

#[test]
fn a_show_after_a_minute_lists_the_bin_again() {
    let s = setup(vec![
        tree(10, &[file(5, "NOTE/Note/a.note", MD5_A)], None),
        events(&[], 11),
        tree(20, &[file(5, "NOTE/Note/a.note", MD5_A)], None),
    ]);
    s.list(NOTE, None);
    s.follow();
    take(&s);
    s.library.set_visible(false);
    s.time.pass(61_000);
    s.follow();
    assert_eq!(recycles(&s), 1);
    assert_eq!(s.targets().last().unwrap(), "/tree?path=NOTE%2FNote");
}

#[test]
fn restore_into_a_taken_path_asks_for_a_name() {
    let s = setup(vec![
        error(
            "409 Conflict",
            json!({"error": "name_taken", "existing": "B.note"}),
        ),
        error(
            "409 Conflict",
            json!({"error": "name_taken", "existing": "c.note"}),
        ),
        ok(json!({"id": 52, "path": "NOTE/Note/b 2.note", "kind": "file", "seqs": [30]})),
        error(
            "409 Conflict",
            json!({"error": "name_taken", "existing": "c.note"}),
        ),
    ]);
    take(&s);

    let asked = s.library.restore(KEY, None);
    assert_eq!(
        asked,
        Job {
            id: 1,
            kind: JobKind::Change,
            label: "Restore b.note".into(),
            done: 0,
            total: None,
            state: JobState::Asking,
            text: Some("There is already B.note here.".into()),
            question: Some(Clash {
                name: "b.note".into(),
                existing: "B.note".into(),
                can_replace: false,
            }),
            result: None,
            drop_id: None,
        }
    );
    let (method, target, body) = request(&s);
    assert_eq!(
        (method.as_str(), target.as_str()),
        ("POST", "/recycle/restore")
    );
    assert_eq!(json_body(&body), json!({"key": KEY}));
    assert!(s.server.received().is_empty());
    let sent = take(&s);
    assert!(!sent.contains(&LibraryEvent::Recycle));
    assert_eq!(sent.last(), Some(&LibraryEvent::Job { job: asked.clone() }));

    // A name that is not allowed: the job asks again, with the same clash.
    let again = s.library.restore(KEY, Some("NOTE/Note/.b.note"));
    assert_eq!(
        again,
        Job {
            text: Some("This name is not allowed.".into()),
            ..asked.clone()
        }
    );
    assert!(s.server.received().is_empty());

    // A path in another folder: the item stays in its folder, the job asks again, nothing is
    // sent (the server would make the missing folders).
    for path in ["NOTE/Note/sub/b.note", "NOTE/b.note", "b.note"] {
        let again = s.library.restore(KEY, Some(path));
        assert_eq!(
            again,
            Job {
                text: Some("Restore keeps the item in its folder. Type a name without /.".into()),
                ..asked.clone()
            }
        );
    }
    assert!(s.server.received().is_empty());

    // A name that is taken too: the job asks again for it.
    let again = s.library.restore(KEY, Some("NOTE/Note/c.note"));
    assert_eq!((again.id, again.state), (1, JobState::Asking));
    assert_eq!(
        again.question,
        Some(Clash {
            name: "c.note".into(),
            existing: "c.note".into(),
            can_replace: false,
        })
    );
    assert_eq!(
        json_body(&request(&s).2),
        json!({"key": KEY, "path": "NOTE/Note/c.note"})
    );

    // The window asks for a name and sends the path: the same job goes on.
    let job = s.library.restore(KEY, Some("NOTE/Note/b 2.note"));
    assert_eq!(job, done(1, "Restore b.note", Some("NOTE/Note/b 2.note")));
    assert_eq!(
        json_body(&request(&s).2),
        json!({"key": KEY, "path": "NOTE/Note/b 2.note"})
    );

    // An asking job ends when the library binds again.
    let other = s.library.restore("1763290000001/NOTE/Note/c.note", None);
    assert_eq!((other.id, other.state), (2, JobState::Asking));
    take(&s);
    s.library.relogin();
    let cancelled = jobs(&s);
    assert_eq!(cancelled.len(), 1);
    assert_eq!(
        (cancelled[0].id, cancelled[0].state),
        (2, JobState::Cancelled)
    );
    assert_eq!(cancelled[0].question, None);
}

#[test]
fn the_bin_lists_again_after_a_change() {
    let s = setup(vec![
        tree(10, &[file(5, "NOTE/Note/a.note", MD5_A)], None),
        // The app's own delete.
        entry(&file(5, "NOTE/Note/a.note", MD5_A)),
        ok(json!({"id": 5, "path": "NOTE/Note/a.note", "seqs": [11], "removed": []})),
        // The follower reads it: the window heard of it already.
        events(&[ev(11, "file.deleted", 5, "NOTE/Note/a.note")], 12),
        // Deletes of others, also in folders the window does not list: once per batch.
        events(
            &[
                ev(12, "file.deleted", 6, "DOCUMENT/Document/x.pdf"),
                ev(13, "folder.deleted", 7, "EXPORT/old"),
            ],
            14,
        ),
        // Other changes leave the bin as it is.
        events(&[ev(14, "file.created", 8, "EXPORT/new.pdf")], 15),
        // After a gap any delete may be missed.
        gap(900, 950),
        tree(950, &[], None),
        // A restore and Empty.
        ok(json!({"id": 52, "path": "NOTE/Note/b.note", "kind": "file", "seqs": [951]})),
        ok(json!({"removed": 2, "bytes": 102_400, "kept": 0})),
        // A delete that fails changes nothing.
        error("404 Not Found", json!({"error": "not_found"})),
    ]);
    s.list(NOTE, None);
    take(&s);

    s.library.delete(5);
    assert_eq!(recycles(&s), 1);
    s.follow();
    assert_eq!(recycles(&s), 0);
    s.follow();
    assert_eq!(recycles(&s), 1);
    s.follow();
    assert_eq!(recycles(&s), 0);
    s.follow();
    assert_eq!(recycles(&s), 1);

    assert_eq!(s.library.restore(KEY, None).state, JobState::Done);
    assert_eq!(recycles(&s), 1);
    let job = s.library.empty_bin();
    assert_eq!(job, done(3, "Empty the recycle bin", None));
    assert_eq!(recycles(&s), 1);
    assert_eq!(s.library.delete(5).state, JobState::Failed);
    assert_eq!(recycles(&s), 0);

    let targets = s.targets();
    assert_eq!(
        targets,
        [
            "/tree?path=NOTE%2FNote",
            "/entries/5",
            "/entries/5",
            "/events?from=11",
            "/events?from=12",
            "/events?from=14",
            "/events?from=15",
            "/tree?path=NOTE%2FNote",
            "/recycle/restore",
            "/recycle/empty",
            "/entries/5",
        ]
    );
}

#[test]
fn empty_sends_no_body() {
    let s = setup(vec![ok(json!({"removed": 0, "bytes": 0, "kept": 0}))]);
    assert_eq!(s.library.empty_bin().state, JobState::Done);
    let (method, target, body) = request(&s);
    assert_eq!(
        (method.as_str(), target.as_str()),
        ("POST", "/recycle/empty")
    );
    assert!(body.is_empty());
}
