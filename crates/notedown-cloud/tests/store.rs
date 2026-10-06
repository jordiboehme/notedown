//! The file store: wire paths, the id registry, the hash cache, listings, native folders, name
//! collisions and the store lock.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use notedown_cloud::Error;
use notedown_cloud::auth::Accounts;
use notedown_cloud::store::{Entry, Kind, NATIVE_FOLDERS, Store, WirePath};

const SLUG: &str = "jordi";

fn setup() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let accounts = Accounts::new(dir.path());
    accounts
        .create(SLUG, "jordi@example.org", "secret")
        .unwrap();
    let store = Store::open(dir.path(), SLUG).unwrap();
    (dir, store)
}

fn account_dir(data: &Path) -> PathBuf {
    Accounts::new(data).account_dir(SLUG)
}

fn files_dir(data: &Path) -> PathBuf {
    account_dir(data).join("files")
}

fn wp(s: &str) -> WirePath {
    WirePath::parse(s).unwrap()
}

fn put(data: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let path = files_dir(data).join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}

fn set_mtime(path: &Path, t: SystemTime) {
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(t).unwrap();
}

fn paths(entries: &[Entry]) -> Vec<String> {
    entries.iter().map(|e| e.path_display.clone()).collect()
}

fn md5_hex(bytes: &[u8]) -> String {
    use md5::{Digest, Md5};
    const_hex::encode(Md5::digest(bytes))
}

// ---------------------------------------------------------------- paths

#[test]
fn wire_paths_normalize_every_device_form() {
    for form in [
        "/NOTE/Note/x.note",
        "NOTE/Note/x.note",
        "NOTE/Note//x.note",
        "//NOTE/Note/x.note",
        "/NOTE/Note/x.note/",
    ] {
        let p = wp(form);
        assert_eq!(p.segments(), ["NOTE", "Note", "x.note"], "{form}");
        assert_eq!(p.display(), "NOTE/Note/x.note", "{form}");
        assert_eq!(p.parent_path().as_deref(), Some("/NOTE/Note"), "{form}");
        assert_eq!(p.name(), Some("x.note"));
    }
    let top = wp("/NOTE/");
    assert_eq!(top.display(), "NOTE");
    assert_eq!(top.parent_path(), None);
    assert_eq!(wp("NOTE/Note/").parent().unwrap(), top);
}

#[test]
fn the_root_parses_and_named_paths_refuse_it() {
    for form in ["", "/", "//"] {
        assert!(wp(form).is_root(), "{form:?}");
        assert!(matches!(
            WirePath::parse_named(form),
            Err(Error::Invalid { .. })
        ));
    }
    assert_eq!(WirePath::root().display(), "");
}

#[test]
fn wire_paths_refuse_escapes_and_bad_bytes() {
    for bad in [
        "/NOTE/../etc/passwd",
        "..",
        "NOTE/./x.note",
        "NOTE/Note/x\0.note",
        "NOTE\\Note\\x.note",
        "/../../x",
    ] {
        assert!(
            matches!(WirePath::parse(bad), Err(Error::Invalid { .. })),
            "{bad:?} must be refused"
        );
    }
}

#[test]
fn nfd_names_round_trip_byte_exact() {
    let nfd = "NOTE/Note/U\u{308}berleben.note";
    let nfc = "NOTE/Note/\u{dc}berleben.note";
    let p = wp(nfd);
    assert_eq!(p.display(), nfd);
    assert_eq!(p.display().as_bytes(), nfd.as_bytes());
    assert_ne!(p, wp(nfc));
}

#[test]
fn join_and_hidden_segments() {
    let p = WirePath::root().join("NOTE").unwrap().join("Note").unwrap();
    assert_eq!(p.display(), "NOTE/Note");
    assert!(p.join("..").is_err());
    assert!(p.join("a/b").is_err());
    assert!(wp("NOTE/.x/y").is_hidden());
    assert!(!wp("NOTE/x.y").is_hidden());
    assert!(wp("NOTE/Note/Work/x").starts_with(&wp("NOTE/Note/Work")));
    assert!(!wp("NOTE/Note/Workshop/x").starts_with(&wp("NOTE/Note/Work")));
}

// ---------------------------------------------------------------- ids

#[test]
fn ids_start_at_1000_and_are_stable() {
    let (_dir, store) = setup();
    let g = store.lock().unwrap();
    let a = store.assign(&g, &wp("NOTE"), Kind::Folder).unwrap();
    let b = store
        .assign(&g, &wp("NOTE/Note/x.note"), Kind::File)
        .unwrap();
    assert_eq!((a, b), (1000, 1001));
    // assigning a known path again keeps its id
    assert_eq!(
        store
            .assign(&g, &wp("NOTE/Note/x.note"), Kind::File)
            .unwrap(),
        1001
    );
    assert_eq!(store.id_of(&wp("/NOTE/Note//x.note")).unwrap(), Some(1001));
    assert_eq!(store.path_of(1001).unwrap(), Some(wp("NOTE/Note/x.note")));
    assert_eq!(store.path_of(4711).unwrap(), None);
}

#[test]
fn a_folder_move_renames_every_descendant_and_nothing_else() {
    let (_dir, store) = setup();
    let g = store.lock().unwrap();
    let work = store
        .assign(&g, &wp("NOTE/Note/Work"), Kind::Folder)
        .unwrap();
    let x = store
        .assign(&g, &wp("NOTE/Note/Work/x.note"), Kind::File)
        .unwrap();
    let deep = store
        .assign(&g, &wp("NOTE/Note/Work/Sub/y.note"), Kind::File)
        .unwrap();
    let shop = store
        .assign(&g, &wp("NOTE/Note/Workshop/z.note"), Kind::File)
        .unwrap();
    store.moved(&g, work, &wp("NOTE/Note/Job")).unwrap();
    assert_eq!(store.path_of(work).unwrap(), Some(wp("NOTE/Note/Job")));
    assert_eq!(store.path_of(x).unwrap(), Some(wp("NOTE/Note/Job/x.note")));
    assert_eq!(
        store.path_of(deep).unwrap(),
        Some(wp("NOTE/Note/Job/Sub/y.note"))
    );
    assert_eq!(
        store.path_of(shop).unwrap(),
        Some(wp("NOTE/Note/Workshop/z.note"))
    );
    assert_eq!(store.id_of(&wp("NOTE/Note/Work/x.note")).unwrap(), None);
    assert_eq!(store.id_of(&wp("NOTE/Note/Job/x.note")).unwrap(), Some(x));
}

#[test]
fn a_file_move_keeps_its_id_and_a_copy_gets_a_new_one() {
    let (_dir, store) = setup();
    let g = store.lock().unwrap();
    let x = store.assign(&g, &wp("INBOX/a.pdf"), Kind::File).unwrap();
    store.moved(&g, x, &wp("DOCUMENT/Document/a.pdf")).unwrap();
    assert_eq!(
        store.id_of(&wp("DOCUMENT/Document/a.pdf")).unwrap(),
        Some(x)
    );
    let copy = store.assign(&g, &wp("INBOX/a.pdf"), Kind::File).unwrap();
    assert_ne!(copy, x);
    assert!(copy > x);
}

#[test]
fn removed_ids_are_never_reused_even_after_restart() {
    let (dir, store) = setup();
    {
        let g = store.lock().unwrap();
        assert_eq!(store.assign(&g, &wp("A/a"), Kind::File).unwrap(), 1000);
        assert_eq!(store.assign(&g, &wp("A/b"), Kind::File).unwrap(), 1001);
        store.removed(&g, 1001).unwrap();
        assert_eq!(store.path_of(1001).unwrap(), None);
        assert_eq!(store.id_of(&wp("A/b")).unwrap(), None);
    }
    drop(store);
    // restart without compaction in between: the log alone must keep the counter
    let store = Store::open(dir.path(), SLUG).unwrap();
    let g = store.lock().unwrap();
    assert_eq!(store.assign(&g, &wp("A/c"), Kind::File).unwrap(), 1002);
    drop(g);
    drop(store);
    // and again after the compaction of the first restart
    let store = Store::open(dir.path(), SLUG).unwrap();
    let g = store.lock().unwrap();
    assert_eq!(store.assign(&g, &wp("A/d"), Kind::File).unwrap(), 1003);
}

#[test]
fn removing_a_folder_drops_its_descendants() {
    let (_dir, store) = setup();
    let g = store.lock().unwrap();
    let f = store.assign(&g, &wp("A/F"), Kind::Folder).unwrap();
    let x = store.assign(&g, &wp("A/F/x"), Kind::File).unwrap();
    let y = store.assign(&g, &wp("A/Fy"), Kind::File).unwrap();
    store.removed(&g, f).unwrap();
    assert_eq!(store.path_of(x).unwrap(), None);
    assert_eq!(store.path_of(y).unwrap(), Some(wp("A/Fy")));
}

#[test]
fn restart_keeps_ids_and_compacts_the_log() {
    let (dir, store) = setup();
    let acc = account_dir(dir.path());
    {
        let g = store.lock().unwrap();
        let f = store.assign(&g, &wp("NOTE/Note"), Kind::Folder).unwrap();
        store
            .assign(&g, &wp("NOTE/Note/x.note"), Kind::File)
            .unwrap();
        store.moved(&g, f, &wp("NOTE/Other")).unwrap();
        let gone = store.assign(&g, &wp("INBOX/z"), Kind::File).unwrap();
        store.removed(&g, gone).unwrap();
    }
    let log = std::fs::read_to_string(acc.join("ids.jsonl")).unwrap();
    assert_eq!(log.lines().count(), 5, "{log}");
    assert!(log.contains(r#""op":"move""#), "{log}");
    drop(store);

    let store = Store::open(dir.path(), SLUG).unwrap();
    assert_eq!(store.id_of(&wp("NOTE/Other")).unwrap(), Some(1000));
    assert_eq!(store.id_of(&wp("NOTE/Other/x.note")).unwrap(), Some(1001));
    assert_eq!(store.path_of(1002).unwrap(), None);
    // compacted: the log is empty and ids.json holds the state, two-space indent, keyed by id
    let log = std::fs::read_to_string(acc.join("ids.jsonl")).unwrap_or_default();
    assert!(log.is_empty(), "{log}");
    let json = std::fs::read_to_string(acc.join("ids.json")).unwrap();
    assert!(json.ends_with("}\n"));
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["next"], 1003);
    assert_eq!(v["ids"]["1000"]["kind"], "folder");
    assert_eq!(v["ids"]["1001"]["path"], "NOTE/Other/x.note");
    assert!(json.contains("\n  \"next\": 1003"), "{json}");
}

#[test]
fn a_torn_last_log_line_is_dropped_and_appends_continue() {
    let (dir, store) = setup();
    {
        let g = store.lock().unwrap();
        store.assign(&g, &wp("A/a"), Kind::File).unwrap();
    }
    drop(store);
    let log = account_dir(dir.path()).join("ids.jsonl");
    let mut text = std::fs::read_to_string(&log).unwrap();
    text.push_str(r#"{"op":"assign","id":1001,"ki"#);
    std::fs::write(&log, text).unwrap();
    let store = Store::open(dir.path(), SLUG).unwrap();
    let g = store.lock().unwrap();
    assert_eq!(store.id_of(&wp("A/a")).unwrap(), Some(1000));
    let b = store.assign(&g, &wp("A/b"), Kind::File).unwrap();
    assert_eq!(b, 1001);
    drop(g);
    drop(store);
    let store = Store::open(dir.path(), SLUG).unwrap();
    assert_eq!(store.id_of(&wp("A/b")).unwrap(), Some(1001));
}

#[test]
fn a_corrupt_complete_log_line_refuses_to_open() {
    let (dir, store) = setup();
    drop(store);
    let log = account_dir(dir.path()).join("ids.jsonl");
    std::fs::write(&log, "this is not json\n").unwrap();
    assert!(Store::open(dir.path(), SLUG).is_err());
}

#[test]
fn a_second_store_sees_ids_assigned_by_another_handle() {
    // the server keeps a Store open while a CLI process assigns ids
    let (dir, server) = setup();
    {
        let g = server.lock().unwrap();
        server.assign(&g, &wp("A/a"), Kind::File).unwrap();
    }
    let cli = Store::open(dir.path(), SLUG).unwrap();
    {
        let g = cli.lock().unwrap();
        assert_eq!(cli.assign(&g, &wp("A/b"), Kind::File).unwrap(), 1001);
    }
    assert_eq!(server.id_of(&wp("A/b")).unwrap(), Some(1001));
    let g = server.lock().unwrap();
    assert_eq!(server.assign(&g, &wp("A/c"), Kind::File).unwrap(), 1002);
}

// ---------------------------------------------------------------- hashes

#[test]
fn md5_is_cached_and_recomputed_on_size_or_mtime_change() {
    let (dir, store) = setup();
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
    let file = put(dir.path(), "INBOX/a.txt", b"aaaa");
    set_mtime(&file, t0);
    let g = store.lock().unwrap();
    let p = wp("INBOX/a.txt");
    assert_eq!(store.md5_of(&g, &p).unwrap(), md5_hex(b"aaaa"));
    let cache = std::fs::read_to_string(account_dir(dir.path()).join("hashes.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&cache).unwrap();
    assert_eq!(v["INBOX/a.txt"]["size"], 4);
    assert_eq!(v["INBOX/a.txt"]["md5"], md5_hex(b"aaaa"));
    assert!(v["INBOX/a.txt"]["mtime_ns"].is_u64());

    // same size and mtime: the cached value is served (proves the cache is used)
    std::fs::write(&file, b"bbbb").unwrap();
    set_mtime(&file, t0);
    assert_eq!(store.md5_of(&g, &p).unwrap(), md5_hex(b"aaaa"));

    // mtime differs: recomputed
    set_mtime(&file, t0 + Duration::from_secs(5));
    assert_eq!(store.md5_of(&g, &p).unwrap(), md5_hex(b"bbbb"));

    // size differs, mtime forced back to the cached one: recomputed
    std::fs::write(&file, b"ccccc").unwrap();
    set_mtime(&file, t0 + Duration::from_secs(5));
    assert_eq!(store.md5_of(&g, &p).unwrap(), md5_hex(b"ccccc"));
}

#[test]
fn the_hash_cache_survives_restart() {
    let (dir, store) = setup();
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
    let file = put(dir.path(), "INBOX/a.txt", b"aaaa");
    set_mtime(&file, t0);
    {
        let g = store.lock().unwrap();
        store.md5_of(&g, &wp("INBOX/a.txt")).unwrap();
    }
    drop(store);
    std::fs::write(&file, b"bbbb").unwrap();
    set_mtime(&file, t0);
    let store = Store::open(dir.path(), SLUG).unwrap();
    let g = store.lock().unwrap();
    assert_eq!(
        store.md5_of(&g, &wp("INBOX/a.txt")).unwrap(),
        md5_hex(b"aaaa")
    );
}

#[test]
fn md5_of_a_missing_or_folder_path_is_an_error() {
    let (dir, store) = setup();
    std::fs::create_dir_all(files_dir(dir.path()).join("INBOX")).unwrap();
    let g = store.lock().unwrap();
    assert!(store.md5_of(&g, &wp("INBOX/none")).is_err());
    assert!(store.md5_of(&g, &wp("INBOX")).is_err());
}

// ---------------------------------------------------------------- listing

#[test]
fn the_listing_is_flat_sorted_and_hides_server_files() {
    let (dir, store) = setup();
    let data = dir.path();
    {
        let g = store.lock().unwrap();
        store.ensure_native_folders(&g).unwrap();
    }
    put(data, "NOTE/Note/b.note", b"bb");
    put(data, "NOTE/Note/Work/a.note", b"a");
    put(data, "NOTE/Note/Work Shop.note", b"w");
    put(data, "NOTE/Note/.nd-1-2.tmp", b"temp");
    put(data, "NOTE/Note/.hidden/x.note", b"x");
    put(data, ".DS_Store", b"x");
    // server folders next to files/
    let acc = account_dir(data);
    std::fs::create_dir_all(acc.join("staging/u1")).unwrap();
    std::fs::write(acc.join("staging/u1/x.note"), b"x").unwrap();
    std::fs::create_dir_all(acc.join("recycle/1/NOTE")).unwrap();

    let g = store.lock().unwrap();
    let all = store.list(&g, &WirePath::root(), true).unwrap();
    assert_eq!(
        paths(&all),
        [
            "DOCUMENT",
            "DOCUMENT/Document",
            "EXPORT",
            "INBOX",
            "NOTE",
            "NOTE/MyStyle",
            "NOTE/Note",
            "NOTE/Note/Work",
            "NOTE/Note/Work/a.note",
            "NOTE/Note/Work Shop.note",
            "NOTE/Note/b.note",
            "SCREENSHOT",
        ]
    );
    for e in &all {
        assert!(!e.path_display.contains("staging"));
        assert!(!e.name.starts_with('.'));
    }
    let b = all
        .iter()
        .find(|e| e.path_display == "NOTE/Note/b.note")
        .unwrap();
    assert_eq!(b.tag, Kind::File);
    assert_eq!(b.size, 2);
    assert_eq!(b.content_hash.as_deref(), Some(md5_hex(b"bb").as_str()));
    assert_eq!(b.parent_path.as_deref(), Some("/NOTE/Note"));
    let note = all.iter().find(|e| e.path_display == "NOTE").unwrap();
    assert_eq!(note.tag, Kind::Folder);
    assert_eq!(note.size, 0);
    assert_eq!(note.content_hash, None);
    assert_eq!(note.parent_path, None);

    // ids are unique, and a second listing returns the same ids
    let mut ids: Vec<u64> = all.iter().map(|e| e.id).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), all.len());
    let again = store.list(&g, &WirePath::root(), true).unwrap();
    assert_eq!(
        all.iter().map(|e| e.id).collect::<Vec<_>>(),
        again.iter().map(|e| e.id).collect::<Vec<_>>()
    );
    for e in &all {
        assert_eq!(store.id_of(&wp(&e.path_display)).unwrap(), Some(e.id));
    }
}

#[test]
fn a_non_recursive_listing_shows_direct_children_only() {
    let (dir, store) = setup();
    put(dir.path(), "NOTE/Note/b.note", b"bb");
    put(dir.path(), "NOTE/Note/Work/a.note", b"a");
    let g = store.lock().unwrap();
    let kids = store.list(&g, &wp("/NOTE/Note/"), false).unwrap();
    assert_eq!(paths(&kids), ["NOTE/Note/Work", "NOTE/Note/b.note"]);
    let deep = store.list(&g, &wp("NOTE/Note"), true).unwrap();
    assert_eq!(
        paths(&deep),
        [
            "NOTE/Note/Work",
            "NOTE/Note/Work/a.note",
            "NOTE/Note/b.note"
        ]
    );
    assert!(store.list(&g, &wp("NOTE/None"), true).unwrap().is_empty());
    assert!(
        store
            .list(&g, &wp("NOTE/Note/b.note"), true)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn entries_serialize_as_entries_vo() {
    let (dir, store) = setup();
    let file = put(dir.path(), "NOTE/Note/x.note", b"abc");
    let t = SystemTime::UNIX_EPOCH + Duration::from_millis(1_791_270_408_123);
    set_mtime(&file, t);
    let g = store.lock().unwrap();
    let e = store.entry(&g, &wp("NOTE/Note/x.note")).unwrap().unwrap();
    let json = serde_json::to_string(&e).unwrap();
    let id = e.id;
    assert_eq!(
        json,
        format!(
            r#"{{"tag":"file","id":"{id}","name":"x.note","path_display":"NOTE/Note/x.note","content_hash":"{}","is_downloadable":true,"size":3,"lastUpdateTime":1791270408123,"parent_path":"/NOTE/Note"}}"#,
            md5_hex(b"abc")
        )
    );
    let top = store.entry(&g, &wp("NOTE")).unwrap().unwrap();
    let v: serde_json::Value = serde_json::to_value(&top).unwrap();
    assert_eq!(v["tag"], "folder");
    assert!(v["id"].is_string());
    assert_eq!(v["size"], 0);
    assert!(v.get("content_hash").is_none());
    assert!(v.get("parent_path").is_none());
    let sub = store.entry(&g, &wp("NOTE/Note")).unwrap().unwrap();
    assert_eq!(sub.parent_path.as_deref(), Some("/NOTE"));
}

#[test]
fn entry_is_none_for_missing_hidden_root_and_wrong_case_paths() {
    let (dir, store) = setup();
    put(dir.path(), "NOTE/Note/x.note", b"abc");
    put(dir.path(), "NOTE/Note/.tmp", b"abc");
    let g = store.lock().unwrap();
    assert!(store.entry(&g, &wp("NOTE/Note/y.note")).unwrap().is_none());
    assert!(store.entry(&g, &wp("NOTE/Note/.tmp")).unwrap().is_none());
    assert!(store.entry(&g, &WirePath::root()).unwrap().is_none());
    // on a case-insensitive file system the OS would find these; the store must not
    assert!(store.entry(&g, &wp("NOTE/note/x.note")).unwrap().is_none());
    assert!(store.entry(&g, &wp("NOTE/Note/X.note")).unwrap().is_none());
    assert!(store.id_of(&wp("NOTE/Note/X.note")).unwrap().is_none());
}

#[cfg(unix)]
#[test]
fn symlinks_are_not_listed() {
    let (dir, store) = setup();
    let target = put(dir.path(), "INBOX/real.pdf", b"x");
    std::os::unix::fs::symlink(&target, files_dir(dir.path()).join("INBOX/link.pdf")).unwrap();
    let g = store.lock().unwrap();
    let all = store.list(&g, &WirePath::root(), true).unwrap();
    assert_eq!(paths(&all), ["INBOX", "INBOX/real.pdf"]);
}

#[test]
fn listing_ids_survive_a_restart() {
    let (dir, store) = setup();
    put(dir.path(), "EXPORT/a.pdf", b"x");
    let first = {
        let g = store.lock().unwrap();
        store.list(&g, &WirePath::root(), true).unwrap()
    };
    drop(store);
    let store = Store::open(dir.path(), SLUG).unwrap();
    let g = store.lock().unwrap();
    let second = store.list(&g, &WirePath::root(), true).unwrap();
    assert_eq!(
        first
            .iter()
            .map(|e| (e.id, &e.path_display))
            .collect::<Vec<_>>(),
        second
            .iter()
            .map(|e| (e.id, &e.path_display))
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------- native folders

#[test]
fn native_folders_are_created_and_listed_when_empty() {
    let (dir, store) = setup();
    let g = store.lock().unwrap();
    store.ensure_native_folders(&g).unwrap();
    store.ensure_native_folders(&g).unwrap(); // idempotent
    for f in NATIVE_FOLDERS {
        assert!(files_dir(dir.path()).join(f).is_dir(), "{f}");
    }
    let all = store.list(&g, &WirePath::root(), true).unwrap();
    assert_eq!(
        paths(&all),
        [
            "DOCUMENT",
            "DOCUMENT/Document",
            "EXPORT",
            "INBOX",
            "NOTE",
            "NOTE/MyStyle",
            "NOTE/Note",
            "SCREENSHOT",
        ]
    );
    assert!(all.iter().all(|e| e.tag == Kind::Folder));
}

// ---------------------------------------------------------------- collisions

fn fs_is_case_sensitive(dir: &Path) -> bool {
    let probe = dir.join("CaseProbe");
    std::fs::write(&probe, b"x").unwrap();
    let sensitive = !dir.join("caseprobe").exists();
    std::fs::remove_file(probe).unwrap();
    sensitive
}

#[test]
fn a_case_variant_is_refused_on_any_file_system() {
    let (dir, store) = setup();
    put(dir.path(), "NOTE/Note/Bike.note", b"x");
    let g = store.lock().unwrap();
    let parent = wp("NOTE/Note");
    match store.check_name(&g, &parent, "bike.note") {
        Err(Error::NameCollision { existing, new }) => {
            assert_eq!(existing, "Bike.note");
            assert_eq!(new, "bike.note");
        }
        other => panic!("expected a collision, got {other:?}"),
    }
    // the same bytes are no collision (a replace), and other names are fine
    store.check_name(&g, &parent, "Bike.note").unwrap();
    store.check_name(&g, &parent, "Bike2.note").unwrap();
    // a missing parent has no siblings
    store.check_name(&g, &wp("NOTE/New"), "x").unwrap();
}

#[test]
fn a_normalization_variant_is_refused() {
    let (dir, store) = setup();
    put(dir.path(), "NOTE/Note/\u{dc}berleben.note", b"x");
    let g = store.lock().unwrap();
    let err = store
        .check_name(&g, &wp("NOTE/Note"), "U\u{308}berleben.note")
        .unwrap_err();
    assert!(matches!(err, Error::NameCollision { .. }), "{err:?}");
    // case and normalization together
    let err = store
        .check_name(&g, &wp("NOTE/Note"), "u\u{308}berleben.NOTE")
        .unwrap_err();
    assert!(matches!(err, Error::NameCollision { .. }), "{err:?}");
}

#[test]
fn on_a_case_sensitive_file_system_both_variants_list_and_a_third_is_refused() {
    let (dir, store) = setup();
    if !fs_is_case_sensitive(&files_dir(dir.path())) {
        eprintln!("skipped: the temp file system is case-insensitive");
        return;
    }
    put(dir.path(), "INBOX/a.txt", b"1");
    put(dir.path(), "INBOX/A.txt", b"2");
    let g = store.lock().unwrap();
    let all = store.list(&g, &wp("INBOX"), false).unwrap();
    assert_eq!(paths(&all), ["INBOX/A.txt", "INBOX/a.txt"]);
    assert!(matches!(
        store.check_name(&g, &wp("INBOX"), "a.TXT"),
        Err(Error::NameCollision { .. })
    ));
}

#[test]
fn check_path_refuses_a_case_variant_of_a_parent_folder() {
    let (dir, store) = setup();
    put(dir.path(), "NOTE/Note/x.note", b"x");
    let g = store.lock().unwrap();
    assert!(matches!(
        store.check_path(&g, &wp("NOTE/note/y.note")),
        Err(Error::NameCollision { .. })
    ));
    store.check_path(&g, &wp("NOTE/Note/y.note")).unwrap();
    store.check_path(&g, &wp("NOTE/Note/New/y.note")).unwrap();
}

#[test]
fn dot_names_are_refused() {
    let (_dir, store) = setup();
    let g = store.lock().unwrap();
    assert!(matches!(
        store.check_name(&g, &wp("INBOX"), ".secret"),
        Err(Error::Invalid { .. })
    ));
    assert!(matches!(
        store.check_path(&g, &wp("INBOX/.x/y")),
        Err(Error::Invalid { .. })
    ));
}

// ---------------------------------------------------------------- lock

#[test]
fn the_store_lock_excludes_a_second_holder() {
    let (dir, store) = setup();
    let other = Store::open(dir.path(), SLUG).unwrap();
    let g = store.lock().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let g2 = other.lock().unwrap();
        tx.send(()).unwrap();
        drop(g2);
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "the second holder got the lock while the first held it"
    );
    drop(g);
    rx.recv_timeout(Duration::from_secs(10))
        .expect("the second holder gets the lock after release");
    handle.join().unwrap();
    assert!(account_dir(dir.path()).join(".store.lock").exists());
}

#[test]
fn a_guard_of_another_account_is_refused() {
    let (dir, store) = setup();
    Accounts::new(dir.path())
        .create("other", "other@example.org", "secret")
        .unwrap();
    let other = Store::open(dir.path(), "other").unwrap();
    let g = other.lock().unwrap();
    assert!(store.assign(&g, &wp("A/a"), Kind::File).is_err());
}

#[test]
fn open_refuses_an_unknown_account() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        Store::open(dir.path(), "nobody"),
        Err(Error::NoAccount(_))
    ));
}
