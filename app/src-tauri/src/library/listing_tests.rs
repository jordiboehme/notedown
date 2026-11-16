//! Listing and following against a scripted server on loopback: the requests are answered in
//! order, a fake clock says when the last poll was, and the follow steps run on the test's own
//! thread (no timer decides a result).

use std::sync::atomic::AtomicU64;
use std::time::Duration;

use serde_json::{Value, json};

use super::tests::{CountingStore, Events, Place, TOKEN, remote_key};
use super::*;
use crate::rest::test_http::{Scripted, answer};

pub(super) const NOTE: &str = "NOTE/Note";
pub(super) const MD5_A: &str = "0cc175b9c0f1b6a831c399e269772661";
const MD5_B: &str = "92eb5ffee6ae2fec3ad71c777531578f";
const MD5_C: &str = "4a8a08f09d37b73795649038408b5f33";

/// A clock the test moves.
#[derive(Clone, Default)]
pub(super) struct Time(Arc<AtomicU64>);

impl Time {
    pub(super) fn clock(&self) -> Clock {
        let now = self.0.clone();
        Arc::new(move || now.load(Ordering::SeqCst))
    }

    pub(super) fn pass(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

pub(super) struct Setup {
    pub(super) server: Scripted,
    pub(super) library: Library,
    pub(super) events: Events,
    pub(super) time: Time,
}

/// A library bound to a scripted server that answers `answers` in order.
pub(super) fn setup(answers: Vec<String>) -> Setup {
    setup_hooked(answers, |_, _, _| {})
}

/// [`setup`], and `hook` runs with the number of each request and the library before the
/// server answers it: a test changes the binding while a call waits.
pub(super) fn setup_hooked(
    answers: Vec<String>,
    mut hook: impl FnMut(usize, &crate::rest::test_http::Request, &Library) + Send + 'static,
) -> Setup {
    let slot: Arc<Mutex<Option<Library>>> = Arc::default();
    let hooked = slot.clone();
    let server = Scripted::start_with(answers, move |n, request| {
        let library = hooked.lock().unwrap().clone();
        if let Some(library) = library {
            hook(n, request, &library);
        }
    });
    let store = CountingStore::with(&remote_key(server.port, "jordi"), TOKEN);
    let place = Place::remote(server.port, Some("jordi"));
    let events = Events::default();
    let time = Time::default();
    // A wait (423 `device_syncing`) moves the clock instead of sleeping.
    let slept = time.clone();
    let sleeper: Sleeper = Arc::new(move |d: Duration| {
        slept.pass(u64::try_from(d.as_millis()).unwrap());
    });
    let library = Library::with_timing(store, place.func(), events.emit(), time.clock(), sleeper);
    library.refresh();
    assert_eq!(library.state().unwrap().state, LibraryState::Ready);
    *slot.lock().unwrap() = Some(library.clone());
    Setup {
        server,
        library,
        events,
        time,
    }
}

impl Setup {
    /// The targets of the requests made so far, without the `/rest/v1` prefix.
    pub(super) fn targets(&self) -> Vec<String> {
        self.server
            .received()
            .into_iter()
            .map(|r| r.target.trim_start_matches("/rest/v1").to_owned())
            .collect()
    }

    /// The folder events sent since the last call.
    fn folders(&self) -> Vec<FolderState> {
        let all = self.events.all();
        self.events.0.lock().unwrap().clear();
        all.into_iter()
            .filter_map(|e| match e {
                LibraryEvent::Folder { folder } => Some(folder),
                _ => None,
            })
            .collect()
    }

    pub(super) fn list(&self, path: &str, cursor: Option<&str>) -> Listed {
        self.library.list(path, cursor, false).unwrap()
    }

    /// One follow step while the window is visible.
    pub(super) fn follow(&self) {
        self.library.set_visible(true);
        self.library.follow_step();
    }
}

pub(super) fn file(id: u64, path: &str, md5: &str) -> Value {
    json!({"id": id, "path": path, "name": path.rsplit('/').next().unwrap(), "kind": "file",
           "size": 10, "md5": md5, "mtimeMs": 1_763_290_000_000_i64, "writable": true})
}

pub(super) fn folder(id: u64, path: &str) -> Value {
    json!({"id": id, "path": path, "name": path.rsplit('/').next().unwrap(), "kind": "folder",
           "size": null, "md5": null, "mtimeMs": 1_763_290_000_000_i64, "writable": true})
}

pub(super) fn tree(seq: u64, entries: &[Value], next: Option<&str>) -> String {
    answer(
        "200 OK",
        &json!({"seq": seq, "entries": entries, "next": next}).to_string(),
    )
}

pub(super) fn events(list: &[Value], next: u64) -> String {
    answer(
        "200 OK",
        &json!({"events": list, "next": next, "end": next - 1, "more": false, "gap": false})
            .to_string(),
    )
}

pub(super) fn entry(value: &Value) -> String {
    answer("200 OK", &value.to_string())
}

pub(super) fn not_found() -> String {
    answer("404 Not Found", r#"{"error":"not_found"}"#)
}

pub(super) fn ev(seq: u64, kind: &str, id: u64, path: &str) -> Value {
    json!({"seq": seq, "ts": 1_763_290_000_000_i64 + seq as i64, "type": kind,
           "origin": "device", "opId": format!("op-{seq}"), "id": id, "path": path})
}

fn with(mut event: Value, fields: Value) -> Value {
    for (k, v) in fields.as_object().unwrap() {
        event[k] = v.clone();
    }
    event
}

fn names(folder: &FolderState) -> Vec<&str> {
    folder.entries.iter().map(|e| e.name.as_str()).collect()
}

fn listed(setup: &Setup, path: &str) -> FolderState {
    setup
        .library
        .inner
        .cache
        .lock()
        .unwrap()
        .listed(&Key::new(path, false))
        .unwrap_or_else(|| panic!("{path} is not listed"))
        .folder
}

fn is_listed(setup: &Setup, path: &str, folders_only: bool) -> bool {
    setup
        .library
        .inner
        .cache
        .lock()
        .unwrap()
        .listed(&Key::new(path, folders_only))
        .is_some()
}

#[test]
fn the_library_lists_a_folder_page_by_page() {
    let s = setup(vec![
        tree(
            10,
            &[file(1, "NOTE/Note/a.note", MD5_A), folder(2, "NOTE/Note/b")],
            Some("t1:b"),
        ),
        tree(12, &[file(3, "NOTE/Note/c.note", MD5_C)], None),
        tree(11, &[folder(2, "NOTE/Note/b")], None),
        events(&[], 13),
    ]);
    let first = s.list(NOTE, None);
    assert_eq!(names(&first.folder), ["a.note", "b"]);
    assert!(!first.folder.complete);
    assert_eq!(first.next.as_deref(), Some("t1:b"));
    assert_eq!(s.folders(), std::slice::from_ref(&first.folder));

    let second = s.list(NOTE, Some("t1:b"));
    assert_eq!(names(&second.folder), ["a.note", "b", "c.note"]);
    assert!(second.folder.complete);
    assert_eq!(second.next, None);
    assert_eq!(
        second.folder.entries[2],
        serde_json::from_value(file(3, "NOTE/Note/c.note", MD5_C)).unwrap()
    );

    // A cursor that is not the folder's next one answers what the folder holds, without a call.
    let again = s.list(NOTE, Some("t1:b"));
    assert_eq!(again, second);

    // The tree: only folders, a listing of its own.
    let tree_listing = s.library.list(NOTE, None, true).unwrap();
    assert_eq!(names(&tree_listing.folder), ["b"]);
    assert!(tree_listing.folder.complete);

    // The follower reads from the lowest seq of the pages (10, then 11), plus one.
    s.follow();
    assert_eq!(
        s.targets(),
        [
            "/tree?path=NOTE%2FNote",
            "/tree?path=NOTE%2FNote&cursor=t1%3Ab",
            "/tree?path=NOTE%2FNote&kind=folder",
            "/events?from=11",
        ]
    );
}

#[test]
fn events_update_the_listed_folder() {
    let sub2_x = file(6, "NOTE/Note/sub2/x.note", MD5_A);
    let s = setup(vec![
        tree(
            10,
            &[
                file(1, "NOTE/Note/a.note", MD5_A),
                file(2, "NOTE/Note/b.note", MD5_A),
                file(3, "NOTE/Note/c.note", MD5_A),
                file(4, "NOTE/Note/d.note", MD5_A),
                folder(5, "NOTE/Note/sub"),
            ],
            None,
        ),
        tree(10, &[file(6, "NOTE/Note/sub/x.note", MD5_A)], None),
        events(
            &[
                with(
                    ev(11, "file.created", 7, "NOTE/Note/e.note"),
                    json!({"md5": MD5_B, "size": 5, "fileKind": "note"}),
                ),
                with(
                    ev(12, "file.modified", 1, "NOTE/Note/a.note"),
                    json!({"md5": MD5_C, "size": 99, "prevMd5": MD5_A}),
                ),
                with(
                    ev(13, "file.moved", 2, "NOTE/Note/bb.note"),
                    json!({"oldPath": "NOTE/Note/b.note"}),
                ),
                with(
                    ev(14, "file.copied", 8, "NOTE/Note/c copy.note"),
                    json!({"sourceId": 3}),
                ),
                ev(15, "file.deleted", 4, "NOTE/Note/d.note"),
                with(
                    ev(16, "folder.moved", 5, "NOTE/Note/sub2"),
                    json!({"oldPath": "NOTE/Note/sub"}),
                ),
                ev(17, "folder.created", 9, "NOTE/Note/new"),
                // A different entry at a path that is taken replaces it.
                with(
                    ev(18, "file.created", 10, "NOTE/Note/c.note"),
                    json!({"md5": MD5_B, "size": 7}),
                ),
            ],
            19,
        ),
        entry(&file(7, "NOTE/Note/e.note", MD5_B)),
        entry(&with(
            file(1, "NOTE/Note/a.note", MD5_C),
            json!({"size": 99}),
        )),
        entry(&file(8, "NOTE/Note/c copy.note", MD5_A)),
        entry(&folder(9, "NOTE/Note/new")),
        entry(&file(10, "NOTE/Note/c.note", MD5_B)),
        events(&[ev(20, "folder.deleted", 5, "NOTE/Note/sub2")], 21),
        not_found(),
    ]);
    s.list(NOTE, None);
    s.list("NOTE/Note/sub", None);
    s.folders();

    s.follow();
    let note = listed(&s, NOTE);
    assert_eq!(
        names(&note),
        [
            "a.note",
            "bb.note",
            "c copy.note",
            "c.note",
            "e.note",
            "new",
            "sub2"
        ]
    );
    let by_name = |name: &str| {
        note.entries
            .iter()
            .find(|e| e.name == name)
            .unwrap()
            .clone()
    };
    assert_eq!(by_name("a.note").md5.as_deref(), Some(MD5_C));
    assert_eq!(by_name("a.note").size, Some(99));
    assert_eq!(by_name("bb.note").id, 2);
    assert_eq!(by_name("bb.note").path, "NOTE/Note/bb.note");
    assert_eq!(by_name("c.note").id, 10);
    assert_eq!(by_name("sub2").path, "NOTE/Note/sub2");
    assert_eq!(by_name("new").kind, EntryKind::Folder);
    // The folder moved: its listing moved along, with the paths below it.
    assert!(!is_listed(&s, "NOTE/Note/sub", false));
    let sub2 = listed(&s, "NOTE/Note/sub2");
    assert_eq!(
        sub2.entries,
        [serde_json::from_value::<Entry>(sub2_x).unwrap()]
    );
    // The batch, then the entries it fetched.
    let sent = s.folders();
    assert_eq!(sent.len(), 4, "{sent:#?}");
    assert_eq!(sent[0].path, NOTE);
    assert_eq!(sent[1], sub2);
    assert_eq!(sent[2].path, "NOTE/Note/sub");
    assert_eq!(sent[2].error.as_deref(), Some(cache::GONE));
    assert_eq!(sent[3], note);

    // A deleted folder is dropped and listed again: it is not there.
    s.follow();
    assert!(!is_listed(&s, "NOTE/Note/sub2", false));
    assert!(names(&listed(&s, NOTE)).iter().all(|n| *n != "sub2"));
    let sent = s.folders();
    assert_eq!(sent.len(), 2, "{sent:#?}");
    assert_eq!(sent[1].path, "NOTE/Note/sub2");
    assert_eq!(sent[1].error.as_deref(), Some(cache::GONE));
    assert_eq!(
        s.targets(),
        [
            "/tree?path=NOTE%2FNote",
            "/tree?path=NOTE%2FNote%2Fsub",
            "/events?from=11",
            "/entries/7",
            "/entries/1",
            "/entries/8",
            "/entries/9",
            "/entries/10",
            "/events?from=19",
            "/tree?path=NOTE%2FNote%2Fsub2",
        ]
    );
}

#[test]
fn a_late_page_does_not_bring_back_a_deleted_entry() {
    let deleted = ev(23, "file.deleted", 2, "NOTE/Note/x.note");
    let s = setup(vec![
        tree(20, &[file(1, "NOTE/Note/a.note", MD5_A)], Some("t1:a")),
        events(std::slice::from_ref(&deleted), 26),
        // Page 2 was read at seq 20, before the delete, and comes after it.
        tree(20, &[file(2, "NOTE/Note/x.note", MD5_A)], None),
        events(&[deleted], 26),
    ]);
    s.list(NOTE, None);
    s.follow();
    let late = s.list(NOTE, Some("t1:a"));
    assert_eq!(names(&late.folder), ["a.note", "x.note"]);
    s.follow();
    assert_eq!(names(&listed(&s, NOTE)), ["a.note"]);
    assert_eq!(
        s.targets(),
        [
            "/tree?path=NOTE%2FNote",
            "/events?from=21",
            "/tree?path=NOTE%2FNote&cursor=t1%3Aa",
            "/events?from=21",
        ]
    );
}

#[test]
fn a_foreign_change_before_an_own_write_is_kept() {
    let s = setup(vec![
        tree(10, &[file(1, "NOTE/Note/x.note", MD5_A)], None),
        events(
            &[
                with(
                    ev(11, "file.modified", 1, "NOTE/Note/x.note"),
                    json!({"md5": MD5_B, "size": 11}),
                ),
                with(
                    ev(12, "file.modified", 1, "NOTE/Note/x.note"),
                    json!({"md5": MD5_C, "size": 12, "origin": "app"}),
                ),
            ],
            13,
        ),
        entry(&with(
            file(1, "NOTE/Note/x.note", MD5_C),
            json!({"size": 12, "mtimeMs": 1_763_290_099_000_i64}),
        )),
    ]);
    s.list(NOTE, None);
    s.folders();
    // The app replaced x.note; the answer said seq 12.
    s.library.wrote(&[12]);
    s.follow();
    // The batch itself ends at the own write, before the fetch of the new mtime.
    let sent = s.folders();
    assert_eq!(sent.len(), 2, "{sent:#?}");
    assert_eq!(sent[0].entries[0].md5.as_deref(), Some(MD5_C));
    assert_eq!(sent[0].entries[0].size, Some(12));
    let x = &listed(&s, NOTE).entries[0];
    assert_eq!(x.md5.as_deref(), Some(MD5_C));
    assert_eq!(x.mtime_ms, 1_763_290_099_000);
    assert_eq!(s.targets()[1..], ["/events?from=11", "/entries/1"]);
}

#[test]
fn own_writes_are_applied_in_order_and_announced_once() {
    let rename = with(
        ev(11, "file.moved", 1, "NOTE/Note/a.note"),
        json!({"oldPath": "NOTE/Note/b.note", "origin": "app"}),
    );
    let s = setup(vec![
        tree(10, &[file(1, "NOTE/Note/b.note", MD5_A)], Some("t1:b")),
        events(std::slice::from_ref(&rename), 12),
        tree(10, &[file(3, "NOTE/Note/c.note", MD5_A)], None),
        events(&[rename], 12),
    ]);
    s.list(NOTE, None);
    s.folders();
    s.library.wrote(&[11]);
    s.follow();
    let sent = s.folders();
    assert_eq!(sent.len(), 1);
    assert_eq!(names(&sent[0]), ["a.note"]);

    // A page read at seq 10 makes the follower read the rename again: it changes nothing and
    // is not announced again.
    s.list(NOTE, Some("t1:b"));
    assert_eq!(s.folders().len(), 2, "loading, then the page");
    s.follow();
    assert_eq!(s.folders(), []);
    assert_eq!(names(&listed(&s, NOTE)), ["a.note", "c.note"]);
    assert_eq!(
        s.targets(),
        [
            "/tree?path=NOTE%2FNote",
            "/events?from=11",
            "/tree?path=NOTE%2FNote&cursor=t1%3Ab",
            "/events?from=11",
        ]
    );
}

#[test]
fn a_move_into_a_listed_folder_from_an_unlisted_one() {
    let s = setup(vec![
        tree(10, &[file(1, "NOTE/Note/a.note", MD5_A)], None),
        tree(10, &[file(20, "NOTE/Big/m.note", MD5_A)], Some("t1:m")),
        events(
            &[
                with(
                    ev(11, "file.moved", 9, "NOTE/Note/z.note"),
                    json!({"oldPath": "NOTE/Other/z.note"}),
                ),
                // Past the loaded pages of NOTE/Big: its page brings it.
                with(
                    ev(12, "file.moved", 10, "NOTE/Big/z.note"),
                    json!({"oldPath": "NOTE/Other/y.note"}),
                ),
                with(
                    ev(13, "file.moved", 11, "NOTE/Big/b.note"),
                    json!({"oldPath": "NOTE/Other/b.note"}),
                ),
            ],
            14,
        ),
        entry(&file(9, "NOTE/Note/z.note", MD5_B)),
        entry(&file(11, "NOTE/Big/b.note", MD5_C)),
    ]);
    s.list(NOTE, None);
    s.list("NOTE/Big", None);
    s.follow();
    let note = listed(&s, NOTE);
    assert_eq!(names(&note), ["a.note", "z.note"]);
    assert_eq!(note.entries[1].md5.as_deref(), Some(MD5_B));
    assert_eq!(names(&listed(&s, "NOTE/Big")), ["b.note", "m.note"]);
    assert_eq!(
        s.targets(),
        [
            "/tree?path=NOTE%2FNote",
            "/tree?path=NOTE%2FBig",
            "/events?from=11",
            "/entries/9",
            "/entries/11",
        ]
    );
}

#[test]
fn a_create_past_the_loaded_pages_waits_for_its_page() {
    let s = setup(vec![
        tree(
            10,
            &[
                file(1, "NOTE/Note/a.note", MD5_A),
                file(2, "NOTE/Note/m.note", MD5_A),
            ],
            Some("t1:m"),
        ),
        events(
            &[
                with(
                    ev(11, "file.created", 3, "NOTE/Note/z.note"),
                    json!({"md5": MD5_B, "size": 3}),
                ),
                // Renamed from zz to b between the pages: on no page, and before the cursor.
                with(
                    ev(12, "file.moved", 5, "NOTE/Note/b.note"),
                    json!({"oldPath": "NOTE/Note/zz.note"}),
                ),
            ],
            13,
        ),
        entry(&file(5, "NOTE/Note/b.note", MD5_C)),
        tree(12, &[file(3, "NOTE/Note/z.note", MD5_B)], None),
    ]);
    s.list(NOTE, None);
    s.follow();
    // No hole: z.note is not shown before the page that holds it.
    assert_eq!(names(&listed(&s, NOTE)), ["a.note", "b.note", "m.note"]);
    let last = s.list(NOTE, Some("t1:m"));
    assert_eq!(
        names(&last.folder),
        ["a.note", "b.note", "m.note", "z.note"]
    );
    assert!(last.folder.complete);
    assert_eq!(
        s.targets(),
        [
            "/tree?path=NOTE%2FNote",
            "/events?from=11",
            "/entries/5",
            "/tree?path=NOTE%2FNote&cursor=t1%3Am",
        ]
    );
}

#[test]
fn a_folder_renamed_while_paging_lists_again() {
    let s = setup(vec![
        tree(10, &[file(1, "NOTE/Note/sub/a.note", MD5_A)], Some("t1:a")),
        tree(
            10,
            &[file(2, "NOTE/Note/sub/deep/d.note", MD5_A)],
            Some("t1:d"),
        ),
        tree(
            10,
            &[file(3, "NOTE/Note/sub b/s.note", MD5_A)],
            Some("t1:s"),
        ),
        events(
            &[with(
                ev(11, "folder.moved", 5, "NOTE/Note/sub2"),
                json!({"oldPath": "NOTE/Note/sub"}),
            )],
            12,
        ),
        tree(
            12,
            &[
                file(1, "NOTE/Note/sub2/a.note", MD5_A),
                file(4, "NOTE/Note/sub2/b.note", MD5_A),
            ],
            None,
        ),
        tree(12, &[file(2, "NOTE/Note/sub2/deep/d.note", MD5_A)], None),
    ]);
    s.list("NOTE/Note/sub", None);
    s.list("NOTE/Note/sub/deep", None);
    s.list("NOTE/Note/sub b", None);
    s.folders();
    s.follow();
    // Dropped and listed again from the first page at the new path, the one above it too.
    assert!(!is_listed(&s, "NOTE/Note/sub", false));
    assert!(!is_listed(&s, "NOTE/Note/sub/deep", false));
    assert_eq!(names(&listed(&s, "NOTE/Note/sub2")), ["a.note", "b.note"]);
    assert_eq!(names(&listed(&s, "NOTE/Note/sub2/deep")), ["d.note"]);
    // "sub b" is not below "sub".
    assert_eq!(names(&listed(&s, "NOTE/Note/sub b")), ["s.note"]);
    let gone: Vec<String> = s
        .folders()
        .into_iter()
        .filter(|f| f.error.as_deref() == Some(cache::GONE))
        .map(|f| f.path)
        .collect();
    assert_eq!(gone, ["NOTE/Note/sub", "NOTE/Note/sub/deep"]);
    assert_eq!(
        s.targets()[3..],
        [
            "/events?from=11",
            "/tree?path=NOTE%2FNote%2Fsub2",
            "/tree?path=NOTE%2FNote%2Fsub2%2Fdeep",
        ]
    );
}

#[test]
fn a_gap_lists_again() {
    let s = setup(vec![
        tree(10, &[file(1, "NOTE/Note/a.note", MD5_A)], Some("t1:a")),
        tree(10, &[folder(2, "NOTE/Note/sub")], None),
        answer(
            "200 OK",
            &json!({"events": [], "next": 900, "end": 950, "more": false, "gap": true}).to_string(),
        ),
        tree(950, &[file(1, "NOTE/Note/a.note", MD5_B)], None),
        tree(
            950,
            &[folder(2, "NOTE/Note/sub"), folder(3, "NOTE/Note/sub3")],
            None,
        ),
        events(&[], 951),
    ]);
    s.list(NOTE, None);
    s.library.list(NOTE, None, true).unwrap();
    s.folders();
    s.follow();
    assert_eq!(listed(&s, NOTE).entries[0].md5.as_deref(), Some(MD5_B));
    assert!(listed(&s, NOTE).complete);
    let tree_listing = s.library.list(NOTE, Some("t1:a"), true).unwrap();
    assert_eq!(names(&tree_listing.folder), ["sub", "sub3"]);
    assert_eq!(s.folders().len(), 2);
    s.follow();
    assert_eq!(
        s.targets(),
        [
            "/tree?path=NOTE%2FNote",
            "/tree?path=NOTE%2FNote&kind=folder",
            "/events?from=11",
            "/tree?path=NOTE%2FNote",
            "/tree?path=NOTE%2FNote&kind=folder",
            "/events?from=951",
        ]
    );
}

#[test]
fn a_404_on_a_continuation_page_lists_again() {
    let s = setup(vec![
        tree(10, &[file(1, "NOTE/Note/a.note", MD5_A)], Some("t1:a")),
        not_found(),
        tree(
            15,
            &[
                file(1, "NOTE/Note/a.note", MD5_A),
                file(2, "NOTE/Note/b.note", MD5_A),
            ],
            Some("t1:b"),
        ),
        answer("409 Conflict", r#"{"error":"not_a_folder"}"#),
        not_found(),
        tree(20, &[file(7, "NOTE/x/a.note", MD5_A)], Some("t1:a")),
        answer("400 Bad Request", r#"{"error":"bad_cursor"}"#),
        tree(21, &[file(7, "NOTE/x/a.note", MD5_A)], None),
    ]);
    s.list(NOTE, None);
    // The folder was replaced while the app paged it: the first page again.
    let again = s.list(NOTE, Some("t1:a"));
    assert_eq!(names(&again.folder), ["a.note", "b.note"]);
    assert_eq!(again.next.as_deref(), Some("t1:b"));
    // A file is where the folder was, and the first page finds nothing.
    let err = s.library.list(NOTE, Some("t1:b"), false).unwrap_err();
    assert_eq!(err, LibraryError::Rest(RestError::NotFound));
    assert!(!is_listed(&s, NOTE, false));
    let last = s.folders().pop().unwrap();
    assert_eq!(last.path, NOTE);
    assert_eq!(last.error.as_deref(), Some(cache::GONE));
    // A cursor the server does not take any more starts again too.
    s.list("NOTE/x", None);
    let x = s.list("NOTE/x", Some("t1:a"));
    assert!(x.folder.complete);
    assert_eq!(
        s.targets(),
        [
            "/tree?path=NOTE%2FNote",
            "/tree?path=NOTE%2FNote&cursor=t1%3Aa",
            "/tree?path=NOTE%2FNote",
            "/tree?path=NOTE%2FNote&cursor=t1%3Ab",
            "/tree?path=NOTE%2FNote",
            "/tree?path=NOTE%2Fx",
            "/tree?path=NOTE%2Fx&cursor=t1%3Aa",
            "/tree?path=NOTE%2Fx",
        ]
    );
}

#[test]
fn following_stops_while_hidden_and_relists_after_a_minute() {
    let s = setup(vec![
        tree(10, &[file(1, "NOTE/Note/a.note", MD5_A)], None),
        events(&[], 11),
        events(&[], 11),
        tree(30, &[file(1, "NOTE/Note/a.note", MD5_B)], None),
        events(&[], 31),
    ]);
    s.list(NOTE, None);
    s.time.pass(1_000);
    s.library.set_visible(true);
    s.library.follow_step();
    assert_eq!(s.targets(), ["/tree?path=NOTE%2FNote", "/events?from=11"]);

    // Hidden: no call.
    s.library.set_visible(false);
    s.time.pass(2_000);
    s.library.follow_step();
    assert_eq!(s.targets(), Vec::<String>::new());

    // Shown again within a minute of the last poll: it follows on.
    s.time.pass(50_000);
    s.library.set_visible(true);
    s.library.follow_step();
    assert_eq!(s.targets(), ["/events?from=11"]);

    // Hidden for more than a minute: shown again, it lists the folders again.
    s.library.set_visible(false);
    s.time.pass(60_001);
    s.library.set_visible(true);
    s.library.follow_step();
    assert_eq!(s.targets(), ["/tree?path=NOTE%2FNote"]);
    assert_eq!(listed(&s, NOTE).entries[0].md5.as_deref(), Some(MD5_B));

    // A second show while visible does not list again.
    s.time.pass(120_000);
    s.library.set_visible(true);
    s.library.follow_step();
    assert_eq!(s.targets(), ["/events?from=31"]);
}

#[test]
fn account_removed_stops_following() {
    let s = setup(vec![
        tree(
            10,
            &[
                file(1, "NOTE/Note/a.note", MD5_A),
                file(2, "NOTE/Note/b.note", MD5_A),
            ],
            None,
        ),
        events(
            &[
                ev(11, "file.deleted", 1, "NOTE/Note/a.note"),
                json!({"seq": 12, "ts": 1, "type": "account.removed", "origin": "cli", "userId": 7}),
            ],
            13,
        ),
        tree(20, &[file(2, "NOTE/Note/b.note", MD5_A)], None),
        events(&[], 21),
    ]);
    s.list(NOTE, None);
    s.follow();
    // The events up to the removal are applied, then it stops.
    assert_eq!(names(&listed(&s, NOTE)), ["b.note"]);
    s.follow();
    s.follow();
    assert_eq!(s.targets(), ["/tree?path=NOTE%2FNote", "/events?from=11"]);

    // A new binding follows again.
    s.library.relogin();
    assert!(!is_listed(&s, NOTE, false));
    s.list(NOTE, None);
    s.follow();
    assert_eq!(s.targets(), ["/tree?path=NOTE%2FNote", "/events?from=21"]);
}

#[test]
fn an_entry_and_the_recycle_bin_are_read() {
    let s = setup(vec![
        entry(&file(41, "NOTE/Note/a.note", MD5_A)),
        answer(
            "200 OK",
            &json!({"items": [
                {"key": "1763290000000/NOTE/Note/b.note", "path": "NOTE/Note/b.note", "name": "b.note",
                 "kind": "file", "deletedAt": 1_763_290_000_000_i64, "size": 20480, "md5": MD5_A},
                {"key": "1763280000000/NOTE/Note/p", "path": "NOTE/Note/p", "name": "p",
                 "kind": "folder", "deletedAt": 1_763_280_000_000_i64, "size": 81920, "files": 4}
            ], "next": "r1:x"})
            .to_string(),
        ),
        answer("200 OK", r#"{"items": [], "next": null}"#),
    ]);
    let got = s.library.entry(41).unwrap();
    assert_eq!(got.path, "NOTE/Note/a.note");
    let bin = s.library.recycle(None).unwrap();
    assert_eq!(bin.items.len(), 2);
    assert_eq!(bin.items[1].files, Some(4));
    assert_eq!(bin.next.as_deref(), Some("r1:x"));
    assert_eq!(
        serde_json::to_value(&bin.items[0]).unwrap(),
        json!({"key": "1763290000000/NOTE/Note/b.note", "path": "NOTE/Note/b.note", "name": "b.note",
               "kind": "file", "deletedAt": 1_763_290_000_000_i64, "size": 20480, "files": null})
    );
    assert_eq!(s.library.recycle(Some("r1:x")).unwrap().items, []);
    assert_eq!(
        s.targets(),
        ["/entries/41", "/recycle", "/recycle?cursor=r1%3Ax"]
    );
}

#[test]
fn the_follower_thread_ends_with_the_library() {
    let s = setup(vec![]);
    let thread = s.library.start_following(Duration::from_secs(3600));
    let weak = Arc::downgrade(&s.library.inner);
    drop(s);
    assert!(
        weak.upgrade().is_none(),
        "the follower keeps the library alive"
    );
    // The thread sees its channel close and ends; a bounded look, no sleep decides it.
    let until = std::time::Instant::now() + Duration::from_secs(5);
    while !thread.is_finished() && std::time::Instant::now() < until {
        std::thread::yield_now();
    }
    assert!(thread.is_finished(), "the follower thread still runs");
    thread.join().unwrap();
}

fn unreachable() -> String {
    answer("502 Bad Gateway", "")
}

pub(super) fn gap(next: u64, end: u64) -> String {
    answer(
        "200 OK",
        &json!({"events": [], "next": next, "end": end, "more": false, "gap": true}).to_string(),
    )
}

#[test]
fn a_gap_while_the_server_does_not_answer_keeps_the_folders() {
    let s = setup(vec![
        tree(10, &[file(1, "NOTE/Note/a.note", MD5_A)], None),
        gap(500, 500),
        unreachable(),
        // Its seq is not valid after the gap: the folder is not followed until it is listed.
        tree(500, &[file(1, "NOTE/Note/a.note", MD5_B)], None),
        events(&[], 501),
    ]);
    s.list(NOTE, None);
    s.folders();
    s.follow();
    let kept = listed(&s, NOTE);
    assert_eq!(names(&kept), ["a.note"]);
    assert_eq!(kept.error.as_deref(), Some("The server does not answer."));
    assert_eq!(s.folders(), [kept]);
    s.time.pass(2_000);
    s.follow();
    s.time.pass(28_000);
    s.follow();
    let again = listed(&s, NOTE);
    assert_eq!(again.entries[0].md5.as_deref(), Some(MD5_B));
    assert_eq!(again.error, None);
    assert_eq!(
        s.targets(),
        [
            "/tree?path=NOTE%2FNote",
            "/events?from=11",
            "/tree?path=NOTE%2FNote",
            "/tree?path=NOTE%2FNote",
            "/events?from=501",
        ]
    );
}

#[test]
fn a_show_after_a_minute_while_unreachable_keeps_the_folders() {
    let s = setup(vec![
        tree(10, &[file(1, "NOTE/Note/a.note", MD5_A)], None),
        events(&[], 11),
        unreachable(),
        events(&[ev(11, "file.deleted", 1, "NOTE/Note/a.note")], 12),
    ]);
    s.list(NOTE, None);
    s.follow();
    s.library.set_visible(false);
    s.time.pass(60_001);
    s.library.set_visible(true);
    s.library.follow_step();
    let kept = listed(&s, NOTE);
    assert_eq!(names(&kept), ["a.note"]);
    assert_eq!(kept.error.as_deref(), Some("The server does not answer."));
    // It follows on from where the folder was.
    s.library.follow_step();
    assert!(listed(&s, NOTE).entries.is_empty());
    // The follower keeps it current: the error of the failed listing is gone.
    assert_eq!(listed(&s, NOTE).error, None);
    assert_eq!(
        s.targets(),
        [
            "/tree?path=NOTE%2FNote",
            "/events?from=11",
            "/tree?path=NOTE%2FNote",
            "/events?from=11",
        ]
    );
}

#[test]
fn a_dropped_folder_that_cannot_be_listed_is_tried_again() {
    let s = setup(vec![
        tree(10, &[file(1, "NOTE/Note/sub/a.note", MD5_A)], Some("t1:a")),
        events(
            &[with(
                ev(11, "folder.moved", 5, "NOTE/Note/sub2"),
                json!({"oldPath": "NOTE/Note/sub"}),
            )],
            12,
        ),
        unreachable(),
        unreachable(),
        tree(12, &[file(1, "NOTE/Note/sub2/a.note", MD5_A)], None),
        events(&[], 13),
    ]);
    s.list("NOTE/Note/sub", None);
    s.folders();
    s.follow();
    let sent = s.folders();
    assert_eq!(sent.last().unwrap().path, "NOTE/Note/sub2");
    assert_eq!(
        sent.last().unwrap().error.as_deref(),
        Some("The server does not answer.")
    );
    assert!(!is_listed(&s, "NOTE/Note/sub2", false));
    // It fails again the same way: the window is not told again.
    s.follow();
    assert_eq!(s.folders(), []);
    s.follow();
    assert_eq!(names(&listed(&s, "NOTE/Note/sub2")), ["a.note"]);
    assert_eq!(
        s.targets()[1..],
        [
            "/events?from=11",
            "/tree?path=NOTE%2FNote%2Fsub2",
            "/tree?path=NOTE%2FNote%2Fsub2",
            "/tree?path=NOTE%2FNote%2Fsub2",
            "/events?from=13",
        ]
    );
}

#[test]
fn a_410_of_the_old_binding_does_not_stop_the_new_one() {
    let s = setup_hooked(
        vec![
            tree(10, &[file(1, "NOTE/Note/a.note", MD5_A)], None),
            answer(
                "410 Gone",
                r#"{"error":"account_removed","userId":7,"seq":10}"#,
            ),
            tree(20, &[file(1, "NOTE/Note/a.note", MD5_A)], None),
            events(&[], 21),
        ],
        |n, _, library| {
            // A login while the old binding's poll waits for its answer.
            if n == 1 {
                library.relogin();
            }
        },
    );
    s.list(NOTE, None);
    s.follow();
    s.list(NOTE, None);
    s.follow();
    assert_eq!(
        s.targets(),
        [
            "/tree?path=NOTE%2FNote",
            "/events?from=11",
            "/tree?path=NOTE%2FNote",
            "/events?from=21",
        ]
    );
}

#[test]
fn a_list_that_crosses_a_binding_says_so() {
    let s = setup_hooked(
        vec![
            tree(10, &[file(1, "NOTE/Note/a.note", MD5_A)], Some("t1:a")),
            tree(12, &[file(2, "NOTE/Note/b.note", MD5_A)], None),
            tree(20, &[file(3, "NOTE/x/c.note", MD5_A)], None),
        ],
        |n, _, library| {
            if n == 1 || n == 2 {
                library.relogin();
            }
        },
    );
    s.list(NOTE, None);
    let err = s.library.list(NOTE, Some("t1:a"), false).unwrap_err();
    assert_eq!(err, LibraryError::Changed);
    assert_eq!(err.to_string(), "The library changed. List again.");
    assert!(!is_listed(&s, NOTE, false));
    let err = s.library.list("NOTE/x", None, false).unwrap_err();
    assert_eq!(err, LibraryError::Changed);
    assert!(!is_listed(&s, "NOTE/x", false));
    // One call each, no second one.
    assert_eq!(
        s.targets(),
        [
            "/tree?path=NOTE%2FNote",
            "/tree?path=NOTE%2FNote&cursor=t1%3Aa",
            "/tree?path=NOTE%2Fx",
        ]
    );
}

#[test]
fn only_the_shown_folders_are_kept_and_listed_again() {
    let s = setup(vec![
        tree(10, &[file(1, "NOTE/Note/a.note", MD5_A)], None),
        tree(10, &[folder(2, "NOTE/Note/sub")], None),
        tree(10, &[file(3, "NOTE/Other/o.note", MD5_A)], None),
        gap(500, 500),
        tree(500, &[file(1, "NOTE/Note/a.note", MD5_B)], None),
        tree(500, &[folder(2, "NOTE/Note/sub")], None),
    ]);
    s.list(NOTE, None);
    s.library.list(NOTE, None, true).unwrap();
    s.list("NOTE/Other", None);
    s.library
        .set_shown(&[Key::new(NOTE, false), Key::new(NOTE, true)]);
    // The window does not show NOTE/Other any more: it is no longer kept.
    assert!(!is_listed(&s, "NOTE/Other", false));
    s.follow();
    assert_eq!(listed(&s, NOTE).entries[0].md5.as_deref(), Some(MD5_B));
    assert_eq!(
        s.targets()[3..],
        [
            "/events?from=11",
            "/tree?path=NOTE%2FNote",
            "/tree?path=NOTE%2FNote&kind=folder",
        ]
    );
}

#[test]
fn a_folder_that_keeps_failing_after_a_gap_waits_while_the_others_are_followed() {
    let s = setup(vec![
        tree(10, &[file(1, "NOTE/A/a.note", MD5_A)], None),
        tree(10, &[file(2, "NOTE/B/b.note", MD5_A)], None),
        gap(500, 500),
        tree(500, &[file(1, "NOTE/A/a.note", MD5_B)], None),
        unreachable(),
        events(&[], 502),
        unreachable(),
        events(&[], 503),
        events(&[], 504),
        tree(600, &[file(2, "NOTE/B/b.note", MD5_B)], None),
        events(&[], 505),
    ]);
    s.list("NOTE/A", None);
    s.list("NOTE/B", None);
    s.follow();
    assert_eq!(
        listed(&s, "NOTE/B").error.as_deref(),
        Some("The server does not answer.")
    );
    // The others are followed from their new seq; B waits 30 s, then 60 s, then is listed.
    s.time.pass(2_000);
    s.follow();
    s.time.pass(28_000);
    s.follow();
    s.time.pass(30_000);
    s.follow();
    s.time.pass(30_000);
    s.follow();
    let b = listed(&s, "NOTE/B");
    assert_eq!(b.entries[0].md5.as_deref(), Some(MD5_B));
    assert_eq!(b.error, None);
    assert_eq!(
        s.targets()[2..],
        [
            "/events?from=11",
            "/tree?path=NOTE%2FA",
            "/tree?path=NOTE%2FB",
            "/events?from=501",
            "/tree?path=NOTE%2FB",
            "/events?from=502",
            "/events?from=503",
            "/tree?path=NOTE%2FB",
            "/events?from=504",
        ]
    );
}

#[test]
fn the_window_keeps_what_it_listed_until_it_says_otherwise() {
    let s = setup(vec![
        tree(10, &[file(1, "NOTE/Note/a.note", MD5_A)], None),
        tree(10, &[file(3, "NOTE/Other/o.note", MD5_A)], None),
        gap(500, 500),
        tree(500, &[file(1, "NOTE/Note/a.note", MD5_B)], None),
        tree(500, &[file(3, "NOTE/Other/o.note", MD5_B)], None),
        tree(500, &[file(4, "NOTE/Note/sub/x.note", MD5_A)], Some("t1:x")),
        events(
            &[with(
                ev(501, "folder.moved", 5, "NOTE/Note/sub2"),
                json!({"oldPath": "NOTE/Note/sub"}),
            )],
            502,
        ),
        entry(&folder(5, "NOTE/Note/sub2")),
        unreachable(),
        events(&[], 503),
    ]);
    s.list(NOTE, None);
    s.library.set_shown(&[Key::new(NOTE, false)]);
    // Listed after the window said what it shows: kept until it says again.
    s.list("NOTE/Other", None);
    s.follow();
    assert_eq!(
        listed(&s, "NOTE/Other").entries[0].md5.as_deref(),
        Some(MD5_B)
    );
    s.library.set_shown(&[Key::new(NOTE, false)]);
    assert!(!is_listed(&s, "NOTE/Other", false));

    // A shown folder that moves while it pages is listed at its new path; when that fails and
    // the window stops showing it, it is not listed again.
    s.list("NOTE/Note/sub", None);
    s.library
        .set_shown(&[Key::new(NOTE, false), Key::new("NOTE/Note/sub", false)]);
    s.follow();
    s.library.set_shown(&[Key::new(NOTE, false)]);
    s.follow();
    assert!(!is_listed(&s, "NOTE/Note/sub2", false));
    assert_eq!(
        s.targets()[2..],
        [
            "/events?from=11",
            "/tree?path=NOTE%2FNote",
            "/tree?path=NOTE%2FOther",
            "/tree?path=NOTE%2FNote%2Fsub",
            "/events?from=501",
            "/entries/5",
            "/tree?path=NOTE%2FNote%2Fsub2",
            "/events?from=502",
        ]
    );
}
