//! App data (digests and to-dos): the journal events of every write, the snapshot and the
//! `app_data_imported` marker of `account.json`.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use notedown_cloud::Clock;
use notedown_cloud::appdata::{AppData, Digest, DigestGroup, Task, TaskGroup};
use notedown_cloud::auth::{Accounts, AppDataImported, Imported};
use notedown_cloud::journal::{Event, EventBody, Journal, JournalOptions, Origin};
use notedown_cloud::store::Store;
use time::OffsetDateTime;

struct TestClock(AtomicI64);

impl Clock for TestClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp_nanos(self.0.load(Ordering::SeqCst) as i128 * 1_000_000)
            .unwrap()
    }
}

/// 2026-10-06T12:00:00Z
const T0: i64 = 1_791_288_000_000;

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    journal: Journal,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        Accounts::new(dir.path())
            .create("jordi", "jordi@example.org", "secret")
            .unwrap();
        let store = Store::open(dir.path(), "jordi").unwrap();
        let journal = Journal::open_with(
            dir.path(),
            JournalOptions {
                max_file_bytes: 1 << 20,
                clock: Arc::new(TestClock(AtomicI64::new(T0))),
            },
        )
        .unwrap();
        Fixture {
            _dir: dir,
            store,
            journal,
        }
    }

    fn events(&self) -> Vec<Event> {
        self.journal
            .read_from(0)
            .unwrap()
            .map(|e| e.unwrap())
            .collect()
    }

    fn data(&self) -> &Path {
        self.store.account_dir().parent().unwrap().parent().unwrap()
    }
}

fn digest(id: u64, unique: &str) -> Digest {
    Digest {
        id,
        content: Some("an excerpt".into()),
        source_path: Some("Note/x.note".into()),
        source_type: Some(2),
        md5_hash: Some("0123456789abcdef0123456789abcdef".into()),
        metadata: Some(format!(
            r#"{{"author":"Jordi","unique_identifier":"{unique}"}}"#
        )),
        ..Digest::default()
    }
}

fn task(id: &str, list: &str, title: &str) -> Task {
    Task {
        task_id: id.into(),
        task_list_id: list.into(),
        title: Some(title.into()),
        status: Some("needsAction".into()),
        ..Task::default()
    }
}

/// The events after `skip`, as (type, origin, device).
fn kinds(events: &[Event]) -> Vec<(&'static str, Origin, Option<String>)> {
    events
        .iter()
        .map(|e| (e.body.type_name(), e.origin, e.device.clone()))
        .collect()
}

#[test]
fn every_digest_write_journals_its_event() {
    let f = Fixture::new();
    let app = AppData::new(&f.store);
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let dev = Some("SN100");
    app.put_digest(
        &g,
        &digest(895_334_188_016_730_112, "aa"),
        Origin::Device,
        dev,
        &f.journal,
    )
    .unwrap();
    let mut changed = digest(895_334_188_016_730_112, "aa");
    changed.comment_str = Some("typed".into());
    app.put_digest(&g, &changed, Origin::Device, dev, &f.journal)
        .unwrap();
    assert!(
        app.delete_digest(&g, 895_334_188_016_730_112, Origin::Device, dev, &f.journal)
            .unwrap()
    );
    assert!(
        !app.delete_digest(&g, 7, Origin::Device, dev, &f.journal)
            .unwrap(),
        "an unknown id deletes nothing"
    );
    let group = DigestGroup {
        id: 5,
        unique_identifier: Some("g1".into()),
        name: Some("Books".into()),
        ..DigestGroup::default()
    };
    app.put_digest_group(&g, &group, Origin::Cli, None, &f.journal)
        .unwrap();
    let mut renamed = group.clone();
    renamed.name = Some("Papers".into());
    app.put_digest_group(&g, &renamed, Origin::Cli, None, &f.journal)
        .unwrap();
    assert!(
        app.delete_digest_group(&g, 5, Origin::Cli, None, &f.journal)
            .unwrap()
    );

    let ev = f.events();
    let serial = Some("SN100".to_string());
    assert_eq!(
        kinds(&ev),
        vec![
            ("digest.created", Origin::Device, serial.clone()),
            ("digest.updated", Origin::Device, serial.clone()),
            ("digest.deleted", Origin::Device, serial),
            ("digest_group.created", Origin::Cli, None),
            ("digest_group.updated", Origin::Cli, None),
            ("digest_group.deleted", Origin::Cli, None),
        ]
    );
    assert!(ev.iter().all(|e| e.account == "jordi"));
    // The event carries the digest's identity (metadata.unique_identifier), the id exactly.
    let line = ev[0].to_line();
    assert!(
        line.contains(
            r#""type":"digest.created","id":895334188016730112,"uniqueIdentifier":"aa","sourcePath":"Note/x.note","sourceType":2,"md5Hash":"0123456789abcdef0123456789abcdef","opId""#
        ),
        "{line}"
    );
    assert!(
        ev[4]
            .to_line()
            .contains(r#""type":"digest_group.updated","id":5,"name":"Papers","opId""#)
    );
    // Lines read back as the same events.
    let again: Event = serde_json::from_str(&line).unwrap();
    assert_eq!(again, ev[0]);
}

#[test]
fn every_task_write_journals_its_event() {
    let f = Fixture::new();
    let app = AppData::new(&f.store);
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let dev = Some("SN100");
    let list = TaskGroup {
        task_list_id: "l1".into(),
        title: Some("Home".into()),
        ..TaskGroup::default()
    };
    app.put_task_group(&g, &list, Origin::Device, dev, &f.journal)
        .unwrap();
    app.put_task_group(&g, &list, Origin::Device, dev, &f.journal)
        .unwrap();
    app.put_task(
        &g,
        &task("t1", "l1", "milk"),
        Origin::Device,
        dev,
        &f.journal,
    )
    .unwrap();
    app.put_tasks(
        &g,
        &[task("t1", "l1", "oat milk"), task("t2", "l1", "bread")],
        Origin::Device,
        dev,
        &f.journal,
    )
    .unwrap();
    assert!(
        app.delete_task(&g, "t2", Origin::Device, dev, &f.journal)
            .unwrap()
    );
    assert!(
        !app.delete_task(&g, "t2", Origin::Device, dev, &f.journal)
            .unwrap(),
        "a tombstone is deleted already"
    );
    assert!(
        app.delete_task_group(&g, "l1", Origin::Device, dev, &f.journal)
            .unwrap()
    );

    let ev = f.events();
    let s = Some("SN100".to_string());
    assert_eq!(
        kinds(&ev),
        vec![
            ("task_group.created", Origin::Device, s.clone()),
            ("task_group.updated", Origin::Device, s.clone()),
            ("task.created", Origin::Device, s.clone()),
            ("task.updated", Origin::Device, s.clone()),
            ("task.created", Origin::Device, s.clone()),
            ("task.deleted", Origin::Device, s.clone()),
            ("task_group.deleted", Origin::Device, s),
        ]
    );
    // `put_tasks` is one batch: one opId.
    assert_eq!(ev[3].op_id, ev[4].op_id);
    assert_ne!(ev[2].op_id, ev[3].op_id);
    // The delete event keeps the title, so nothing is lost when the tombstone is swept.
    assert!(
        ev[5].to_line().contains(
            r#""type":"task.deleted","taskId":"t2","taskListId":"l1","title":"bread","status":"needsAction","opId""#
        ),
        "{}",
        ev[5].to_line()
    );
    assert!(
        ev[0]
            .to_line()
            .contains(r#""type":"task_group.created","taskListId":"l1","title":"Home","opId""#)
    );
}

#[test]
fn snapshot_journals_the_live_digests_and_tasks() {
    let f = Fixture::new();
    let app = AppData::new(&f.store);
    {
        let g = f.store.lock_recovered(&f.journal).unwrap();
        app.put_digest(&g, &digest(10, "live"), Origin::Cli, None, &f.journal)
            .unwrap();
        app.put_digest(&g, &digest(11, "gone"), Origin::Cli, None, &f.journal)
            .unwrap();
        app.delete_digest(&g, 11, Origin::Cli, None, &f.journal)
            .unwrap();
        app.put_digest_group(
            &g,
            &DigestGroup {
                id: 12,
                ..DigestGroup::default()
            },
            Origin::Cli,
            None,
            &f.journal,
        )
        .unwrap();
        app.put_task(&g, &task("t1", "l1", "milk"), Origin::Cli, None, &f.journal)
            .unwrap();
        app.put_task(
            &g,
            &task("t2", "l1", "bread"),
            Origin::Cli,
            None,
            &f.journal,
        )
        .unwrap();
        app.delete_task(&g, "t2", Origin::Cli, None, &f.journal)
            .unwrap();
    }
    let before = f.events().len();
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let seqs = f.store.snapshot(&g, &f.journal).unwrap();
    let ev: Vec<Event> = f.events().into_iter().skip(before).collect();
    assert_eq!(seqs.len(), ev.len());
    assert!(
        ev.iter()
            .all(|e| e.origin == Origin::Snapshot && e.op_id == ev[0].op_id)
    );
    let app_events: Vec<&EventBody> = ev
        .iter()
        .map(|e| &e.body)
        .filter(|b| !b.type_name().starts_with("folder.") && !b.type_name().starts_with("file."))
        .collect();
    assert_eq!(app_events.len(), 2, "{app_events:?}");
    assert_eq!(app_events[0].type_name(), "digest.created");
    assert_eq!(app_events[1].type_name(), "task.created");
    let lines: Vec<String> = ev.iter().map(Event::to_line).collect();
    assert!(
        lines
            .iter()
            .any(|l| l.contains(r#""id":10,"uniqueIdentifier":"live""#))
    );
    assert!(lines.iter().any(|l| l.contains(r#""taskId":"t1""#)));
    // The app data events come after the file events.
    let first_app = ev
        .iter()
        .position(|e| e.body.type_name() == "digest.created")
        .unwrap();
    assert!(
        ev[first_app..]
            .iter()
            .all(|e| !e.body.type_name().starts_with("f"))
    );
}

#[test]
fn snapshot_of_an_account_without_app_data_is_unchanged() {
    let f = Fixture::new();
    let g = f.store.lock_recovered(&f.journal).unwrap();
    f.store.snapshot(&g, &f.journal).unwrap();
    assert!(f.events().iter().all(|e| {
        let t = e.body.type_name();
        t.starts_with("folder.") || t.starts_with("file.")
    }));
}

#[test]
fn account_json_without_the_app_data_marker_still_loads() {
    let f = Fixture::new();
    let accounts = Accounts::new(f.data());
    let path = accounts.account_dir("jordi").join("account.json");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("app_data_imported"));
    let mut a = accounts.get("jordi").unwrap().unwrap();
    assert_eq!(a.app_data_imported, None);

    a.imported = Some(Imported {
        at: "2026-10-06T12:00:00Z".into(),
        source: "/backup".into(),
        files: 3,
        bytes: 30,
    });
    a.app_data_imported = Some(AppDataImported {
        at: "2026-10-07T12:00:00Z".into(),
        source: "/backup/x.snbak".into(),
        digests: 6,
        groups: 0,
        marks: 5,
    });
    accounts.save(&a).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let imported = text.find("\"imported\"").unwrap();
    let marker = text.find("\"app_data_imported\"").unwrap();
    let locked_at = text.find("\"locked_at_ms\"").unwrap();
    assert!(imported < marker && marker < locked_at, "{text}");
    assert!(text.contains(
        "\"app_data_imported\": {\n    \"at\": \"2026-10-07T12:00:00Z\",\n    \"source\": \"/backup/x.snbak\",\n    \"digests\": 6,\n    \"groups\": 0,\n    \"marks\": 5\n  }"
    ));
    assert_eq!(accounts.get("jordi").unwrap().unwrap(), a);
}

// ---- import-app-data ----

use notedown_cloud::appdata::{AppImportOptions, AppImportReport, import_app_data};
use serde_json::{Value, json};
use std::io::Write;

pub(crate) const MARK_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.mark";
pub(crate) const MARK_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.mark";

/// A fixture whose account holds the device files (a real, tiny `notedown cloud import`).
fn import_fixture() -> Fixture {
    let f = Fixture::new();
    let backup = f.data().join("device-files");
    std::fs::create_dir_all(backup.join("Note")).unwrap();
    std::fs::write(backup.join("Note/a.note"), b"note").unwrap();
    for d in ["Document", "MyStyle", "EXPORT", "SCREENSHOT", "INBOX"] {
        std::fs::create_dir_all(backup.join(d)).unwrap();
    }
    notedown_cloud::import::import(
        f.data(),
        "jordi",
        &backup,
        &notedown_cloud::import::ImportOptions {
            no_report: true,
            ..Default::default()
        },
        &f.journal,
        &mut |_| {},
    )
    .unwrap();
    f
}

/// Write a `.snbak` (a ZIP with deflate) with the given (name, bytes) entries.
pub(crate) fn snbak(path: &Path, entries: &[(&str, Vec<u8>)]) {
    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in entries {
        zip.start_file(*name, options).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap();
}

pub(crate) fn json_bytes(v: Value) -> Vec<u8> {
    serde_json::to_vec(&v).unwrap()
}

/// Two digests (a note with a `.mark` and a group, a document with `source_size` and a
/// `.mark`), one without cloud id, one group.
pub(crate) fn backup_entries() -> Vec<(&'static str, Vec<u8>)> {
    let note_meta = r#"{"author":"Jordi","unique_identifier":"n1","source_size":3344196}"#;
    let doc_meta = r#"{"unique_identifier":"d1"}"#;
    vec![
        (
            "backup/DIGEST/metadata.json",
            json_bytes(json!({"backupVersion": 1})),
        ),
        (
            "backup/DIGEST/knowledge.json",
            json_bytes(json!([
                {"id": 1, "serviceId": 895334188016730112u64,
                 "content": "A line from the note that is longer than forty characters in total",
                 "commentStr": "typed", "commentHandwriteName": MARK_A,
                 "handwriteMD5": "h1", "dataMD5": "m1", "creationTime": 1700000000000i64,
                 "lastModifiedTime": 1700000001000i64, "sourcePath": "Note/x.note",
                 "sourcePage": 3, "sourceType": 2, "state": 0, "syncState": 1,
                 "knowledgeBaseUniqueAttribute": "g1", "metadata": note_meta},
                {"id": 2, "serviceId": 895334188016730200u64, "content": "From the doc\nsecond line",
                 "commentHandwriteName": MARK_B, "handwriteMD5": "h2", "dataMD5": "m2",
                 "creationTime": 1700000002000i64, "lastModifiedTime": 1700000003000i64,
                 "sourcePath": "Document/b.pdf", "sourceType": 1,
                 "knowledgeBaseUniqueAttribute": "", "metadata": doc_meta},
                {"id": 3, "serviceId": 0, "content": "never synced", "commentHandwriteName": "",
                 "sourcePath": "Note/y.note", "sourceType": 2, "metadata": "{}"}
            ])),
        ),
        (
            "backup/DIGEST/knowledge_bases.json",
            json_bytes(json!([
                {"id": 1, "serviceId": 895334188016730300u64, "uniqueIdentifier": "g1",
                 "name": "Ideas", "description": "d", "md5Hash": "gm",
                 "creationTime": 1700000000000i64, "lastModifiedTime": 1700000000500i64}
            ])),
        ),
        (
            "backup/DIGEST/handwrite/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.mark",
            b"mark A".to_vec(),
        ),
        (
            "backup/DIGEST/handwrite/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.mark",
            b"mark B".to_vec(),
        ),
        (
            "backup/DIGEST/handwrite/cccccccccccccccccccccccccccccccc.mark",
            b"unused".to_vec(),
        ),
        ("backup/DIGEST/digest.db", b"sqlite".to_vec()),
        ("backup/Plugin/x", b"x".to_vec()),
    ]
}

/// The digest and to-do events (the file import before them is not of interest).
fn app_events(f: &Fixture) -> Vec<Event> {
    f.events()
        .into_iter()
        .filter(|e| {
            let n = e.body.type_name();
            n.starts_with("digest") || n.starts_with("task")
        })
        .collect()
}

fn run(
    f: &Fixture,
    file: Option<&Path>,
    options: &AppImportOptions,
) -> notedown_cloud::Result<AppImportReport> {
    import_app_data(f.data(), "jordi", file, options, &f.journal)
}

fn no_options() -> AppImportOptions {
    AppImportOptions::default()
}

#[test]
fn app_import_maps_digests_groups_and_marks() {
    let f = import_fixture();
    let file = f.data().join("device.snbak");
    snbak(&file, &backup_entries());
    let r = run(&f, Some(&file), &no_options()).unwrap();
    assert_eq!((r.digests, r.groups, r.marks), (2, 1, 2));
    // The digest without a cloud id is left to the device: it adds it itself.
    assert_eq!(r.left_to_device, vec!["Note/y.note  never synced"]);
    assert!(r.without_group.is_empty(), "{:?}", r.without_group);
    assert_eq!(
        r.skipped,
        vec!["backup/DIGEST/handwrite/cccccccccccccccccccccccccccccccc.mark"]
    );

    let app = AppData::new(&f.store);
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let note = app.digest(&g, 895_334_188_016_730_112).unwrap().unwrap();
    let wire = serde_json::to_value(note.wire()).unwrap();
    assert_eq!(
        wire,
        json!({
            "id": 895334188016730112u64, "fileId": null, "name": null, "userId": 1_000_001,
            "uniqueIdentifier": "", "parentUniqueIdentifier": "g1",
            "content": "A line from the note that is longer than forty characters in total",
            "sourcePath": "Note/x.note", "dataSource": null, "sourceType": 2,
            "isSummaryGroup": "N", "description": null, "tags": null, "md5Hash": "m1",
            "metadata": r#"{"author":"Jordi","unique_identifier":"n1","source_size":3344196}"#,
            "commentStr": "typed", "commentHandwriteName": MARK_A,
            "handwriteInnerName": MARK_A, "handwriteMD5": "h1",
            "creationTime": 1700000000000i64, "lastModifiedTime": 1700000001000i64,
            "isDeleted": "N", "createTime": T0, "updateTime": T0, "author": "Jordi"
        })
    );
    let doc = app.digest(&g, 895_334_188_016_730_200).unwrap().unwrap();
    assert_eq!(doc.author, None);
    assert_eq!(doc.parent_unique_identifier.as_deref(), Some(""));
    assert!(
        doc.metadata
            .as_deref()
            .unwrap()
            .contains("unique_identifier")
    );
    // The digest without a cloud id is not stored.
    let all = app.digests(&g).unwrap();
    assert_eq!(all.len(), 2);
    assert!(
        all.iter()
            .all(|d| d.content.as_deref() != Some("never synced"))
    );

    let groups = app.digest_groups(&g).unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].id, 895_334_188_016_730_300);
    assert_eq!(groups[0].unique_identifier.as_deref(), Some("g1"));
    assert_eq!(groups[0].name.as_deref(), Some("Ideas"));
    assert_eq!(groups[0].md5_hash.as_deref(), Some("gm"));
    assert_eq!(groups[0].last_modified_time, Some(1_700_000_000_500));
    // The backup has no createTime: the device's creationTime stands in; updateTime is now.
    assert_eq!(groups[0].create_time, 1_700_000_000_000);
    assert_eq!(groups[0].update_time, T0 as u64);

    assert_eq!(app.mark(&g, MARK_A).unwrap().unwrap(), b"mark A");
    assert_eq!(app.mark(&g, MARK_B).unwrap().unwrap(), b"mark B");
    assert_eq!(
        app.mark(&g, &format!("{}.mark", "c".repeat(32))).unwrap(),
        None
    );
    drop(g);

    let a = Accounts::new(f.data()).get("jordi").unwrap().unwrap();
    let m = a.app_data_imported.unwrap();
    assert_eq!((m.digests, m.groups, m.marks), (2, 1, 2));
    assert_eq!(m.source, file.display().to_string());

    // One batch per record, origin cli, no device.
    let events = app_events(&f);
    let types: Vec<_> = kinds(&events).into_iter().collect();
    assert_eq!(types.len(), 3);
    assert_eq!(types[0].0, "digest_group.created");
    assert!(types[1..].iter().all(|t| t.0 == "digest.created"));
    assert!(types.iter().all(|t| t.1 == Origin::Cli && t.2.is_none()));
    let ops: std::collections::BTreeSet<_> = events.iter().map(|e| e.op_id.clone()).collect();
    assert_eq!(ops.len(), 3);
}

#[test]
fn app_import_dry_run_writes_nothing_and_prints_the_mapping() {
    let f = import_fixture();
    let file = f.data().join("device.snbak");
    snbak(&file, &backup_entries());
    let r = run(
        &f,
        Some(&file),
        &AppImportOptions {
            dry_run: true,
            replace: false,
        },
    )
    .unwrap();
    assert_eq!((r.digests, r.groups, r.marks), (2, 1, 2));
    assert_eq!(r.mapping.len(), 2);
    assert_eq!(r.left_to_device, vec!["Note/y.note  never synced"]);
    assert_eq!(
        r.mapping[0],
        "895334188016730112  Note/x.note  A line from the note that is longer than"
    );
    assert_eq!(
        r.mapping[1],
        "895334188016730200  Document/b.pdf  From the doc second line"
    );
    assert!(!f.store.account_dir().join("digests").exists());
    assert!(app_events(&f).is_empty());
    let a = Accounts::new(f.data()).get("jordi").unwrap().unwrap();
    assert_eq!(a.app_data_imported, None);
}

#[test]
fn app_import_refuses_without_the_file_marker_with_protect_on_or_off() {
    let f = Fixture::new();
    let file = f.data().join("device.snbak");
    snbak(&file, &backup_entries());
    let e = run(&f, Some(&file), &no_options()).unwrap_err().to_string();
    assert!(
        e.contains("import the device files first (notedown cloud import)"),
        "{e}"
    );
    let e = run(&f, None, &no_options()).unwrap_err().to_string();
    assert!(e.contains("import the device files first"), "{e}");
    assert!(app_events(&f).is_empty());
}

#[test]
fn app_import_refuses_missing_marks_missing_knowledge_and_duplicate_ids() {
    let f = import_fixture();
    let file = f.data().join("device.snbak");

    let mut entries = backup_entries();
    entries.retain(|(n, _)| !n.ends_with(MARK_A) && !n.ends_with(MARK_B));
    snbak(&file, &entries);
    let e = run(&f, Some(&file), &no_options()).unwrap_err().to_string();
    assert!(e.contains(MARK_A) && e.contains(MARK_B), "{e}");

    let mut entries = backup_entries();
    entries.retain(|(n, _)| *n != "backup/DIGEST/knowledge.json");
    snbak(&file, &entries);
    let e = run(&f, Some(&file), &no_options()).unwrap_err().to_string();
    assert!(e.contains("knowledge.json"), "{e}");

    let mut entries = backup_entries();
    for (n, b) in &mut entries {
        if *n == "backup/DIGEST/knowledge.json" {
            *b = json_bytes(json!([
                {"serviceId": 7, "content": "a", "sourcePath": "p"},
                {"serviceId": 7, "content": "b", "sourcePath": "q"}
            ]));
        }
    }
    snbak(&file, &entries);
    let e = run(&f, Some(&file), &no_options()).unwrap_err().to_string();
    assert!(e.contains("more than once") && e.contains('7'), "{e}");

    std::fs::write(&file, b"not a zip").unwrap();
    assert!(run(&f, Some(&file), &no_options()).is_err());

    // Nothing was written by any refusal, and the marker is not set.
    assert!(!f.store.account_dir().join("digests").exists());
    assert!(app_events(&f).is_empty());
    let a = Accounts::new(f.data()).get("jordi").unwrap().unwrap();
    assert_eq!(a.app_data_imported, None);
}

#[test]
fn app_import_refuses_an_account_with_digests_unless_replace_and_leaves_to_dos_alone() {
    let f = import_fixture();
    let file = f.data().join("device.snbak");
    snbak(&file, &backup_entries());
    put_to_dos(&f);
    {
        let app = AppData::new(&f.store);
        let g = f.store.lock_recovered(&f.journal).unwrap();
        app.put_digest(&g, &digest(5, "old"), Origin::Device, None, &f.journal)
            .unwrap();
    }
    let e = run(&f, Some(&file), &no_options()).unwrap_err().to_string();
    assert!(
        e.contains("1 digests and 0 digest groups") && e.contains("--replace"),
        "{e}"
    );
    assert!(!e.contains("to-do"), "{e}");
    assert!(run(&f, None, &no_options()).is_err());
    let e = notedown_cloud::appdata::dry_run_app_data(f.data(), "jordi", Some(&file), false)
        .unwrap_err()
        .to_string();
    assert!(!e.contains("to-do"), "{e}");
    let dry =
        notedown_cloud::appdata::dry_run_app_data(f.data(), "jordi", Some(&file), true).unwrap();
    assert_eq!(dry.replaced, 1);

    let before = app_events(&f).len();
    let r = run(
        &f,
        Some(&file),
        &AppImportOptions {
            dry_run: false,
            replace: true,
        },
    )
    .unwrap();
    assert_eq!(r.replaced, 1);
    let events = app_events(&f);
    let new: Vec<_> = kinds(&events[before..]).into_iter().map(|k| k.0).collect();
    assert_eq!(
        new,
        vec![
            "digest.deleted",
            "digest_group.created",
            "digest.created",
            "digest.created",
        ]
    );
    let app = AppData::new(&f.store);
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let live: Vec<_> = app
        .digests(&g)
        .unwrap()
        .into_iter()
        .filter(|d| d.is_live())
        .collect();
    assert_eq!(live.len(), 2);
    assert!(!app.digest(&g, 5).unwrap().unwrap().is_live());
    drop(g);
    assert_to_dos_untouched(&f);
}

/// A to-do, its list and the list's sort, as the device writes them.
fn put_to_dos(f: &Fixture) {
    use notedown_cloud::appdata::TaskSort;
    let app = AppData::new(&f.store);
    let g = f.store.lock_recovered(&f.journal).unwrap();
    app.put_task(
        &g,
        &task("t1", "l1", "buy"),
        Origin::Device,
        None,
        &f.journal,
    )
    .unwrap();
    app.put_task_group(
        &g,
        &TaskGroup {
            task_list_id: "l1".into(),
            title: Some("Inbox".into()),
            ..TaskGroup::default()
        },
        Origin::Device,
        None,
        &f.journal,
    )
    .unwrap();
    app.put_task_sort(
        &g,
        &TaskSort {
            task_list_id: "l1".into(),
            ..TaskSort::default()
        },
    )
    .unwrap();
}

/// The records of [`put_to_dos`] are live and unchanged, and no import wrote a to-do event.
fn assert_to_dos_untouched(f: &Fixture) {
    let app = AppData::new(&f.store);
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let tasks = app.tasks(&g).unwrap();
    assert_eq!(tasks.len(), 1);
    assert!(tasks[0].is_live());
    assert_eq!(tasks[0].title.as_deref(), Some("buy"));
    let groups = app.task_groups(&g).unwrap();
    assert_eq!(groups.len(), 1);
    assert!(groups[0].is_live());
    assert_eq!(app.task_sorts(&g).unwrap().len(), 1);
    drop(g);
    let cli_task_events = app_events(f)
        .into_iter()
        .filter(|e| e.origin == Origin::Cli && e.body.type_name().starts_with("task"))
        .count();
    assert_eq!(cli_task_events, 0);
}

#[test]
fn app_import_does_not_count_or_touch_to_dos() {
    let f = import_fixture();
    let file = f.data().join("device.snbak");
    snbak(&file, &backup_entries());
    put_to_dos(&f);
    // To-dos alone are no reason to refuse, in the dry run and in the real run.
    let dry =
        notedown_cloud::appdata::dry_run_app_data(f.data(), "jordi", Some(&file), false).unwrap();
    assert_eq!(dry.replaced, 0);
    let r = run(&f, Some(&file), &no_options()).unwrap();
    assert_eq!((r.digests, r.replaced), (2, 0));
    assert_to_dos_untouched(&f);
    // --replace clears the digests, never the to-dos.
    let r = run(
        &f,
        Some(&file),
        &AppImportOptions {
            dry_run: false,
            replace: true,
        },
    )
    .unwrap();
    assert_eq!(r.replaced, 3);
    assert_to_dos_untouched(&f);
}

#[test]
fn app_import_empty_sets_only_the_marker() {
    let f = import_fixture();
    let r = run(&f, None, &no_options()).unwrap();
    assert_eq!(r, AppImportReport::default());
    assert!(app_events(&f).is_empty());
    let a = Accounts::new(f.data()).get("jordi").unwrap().unwrap();
    let m = a.app_data_imported.unwrap();
    assert_eq!(
        (m.source.as_str(), m.digests, m.groups, m.marks),
        ("", 0, 0, 0)
    );
    // A second import is not refused: the account holds no records.
    run(&f, None, &no_options()).unwrap();
}

#[test]
fn app_import_without_groups_file_and_with_an_unreadable_record_in_the_account() {
    let f = import_fixture();
    let file = f.data().join("device.snbak");
    let mut entries = backup_entries();
    entries.retain(|(n, _)| !n.ends_with("knowledge_bases.json"));
    snbak(&file, &entries);
    let r = run(&f, Some(&file), &no_options()).unwrap();
    assert_eq!((r.digests, r.groups), (2, 0));
    // The note's group g1 is not in the backup.
    assert_eq!(
        r.without_group,
        vec!["895334188016730112  Note/x.note  group g1"]
    );
    // A broken record in the account stops a strict read: the import refuses it.
    let items = f.store.account_dir().join("digests/items");
    std::fs::write(items.join("99.json"), b"{broken").unwrap();
    let e = run(
        &f,
        Some(&file),
        &AppImportOptions {
            dry_run: false,
            replace: true,
        },
    );
    assert!(e.is_err());
}

#[test]
fn app_import_dry_run_takes_no_lock_leaves_a_pending_commit_and_creates_no_journal() {
    let f = import_fixture();
    let file = f.data().join("device.snbak");
    snbak(&file, &backup_entries());
    let pending = f.store.account_dir().join("pending.json");
    std::fs::write(&pending, b"an unfinished commit").unwrap();
    std::fs::remove_dir_all(f.data().join("journal")).unwrap();
    let r =
        notedown_cloud::appdata::dry_run_app_data(f.data(), "jordi", Some(&file), false).unwrap();
    assert_eq!((r.digests, r.groups, r.marks), (2, 1, 2));
    assert_eq!(std::fs::read(&pending).unwrap(), b"an unfinished commit");
    assert!(!f.data().join("journal").exists());
    assert!(!f.store.account_dir().join("digests").exists());
    // The options form with dry_run takes the same path.
    let r2 = run(
        &f,
        Some(&file),
        &AppImportOptions {
            dry_run: true,
            replace: false,
        },
    )
    .unwrap();
    assert_eq!(r2, r);
    assert_eq!(std::fs::read(&pending).unwrap(), b"an unfinished commit");
}

#[test]
fn app_import_dry_run_sees_the_records_of_the_account() {
    let f = import_fixture();
    let file = f.data().join("device.snbak");
    snbak(&file, &backup_entries());
    {
        let app = AppData::new(&f.store);
        let g = f.store.lock_recovered(&f.journal).unwrap();
        app.put_digest(
            &g,
            &digest(900_000_000_000_000_000, "old"),
            Origin::Device,
            None,
            &f.journal,
        )
        .unwrap();
    }
    let e = notedown_cloud::appdata::dry_run_app_data(f.data(), "jordi", Some(&file), false)
        .unwrap_err()
        .to_string();
    assert!(e.contains("1 digests") && e.contains("--replace"), "{e}");
    let r =
        notedown_cloud::appdata::dry_run_app_data(f.data(), "jordi", Some(&file), true).unwrap();
    assert_eq!(r.replaced, 1);
    assert_eq!(r.left_to_device, vec!["Note/y.note  never synced"]);
}

#[test]
fn app_import_leaves_a_group_without_service_id_to_the_device() {
    let f = import_fixture();
    let file = f.data().join("device.snbak");
    let mut entries = backup_entries();
    for (n, b) in &mut entries {
        if n.ends_with("knowledge_bases.json") {
            *b = json_bytes(json!([
                {"id": 1, "uniqueIdentifier": "g1", "name": "Ideas", "md5Hash": "gm"}
            ]));
        }
    }
    snbak(&file, &entries);
    let dry =
        notedown_cloud::appdata::dry_run_app_data(f.data(), "jordi", Some(&file), false).unwrap();
    let r = run(&f, Some(&file), &no_options()).unwrap();
    assert_eq!(dry, r);
    assert_eq!((r.digests, r.groups), (2, 0));
    assert_eq!(
        r.left_to_device,
        vec!["Note/y.note  never synced", "group Ideas"]
    );
    // The note names the group that was left out.
    assert_eq!(
        r.without_group,
        vec!["895334188016730112  Note/x.note  group g1"]
    );
    let app = AppData::new(&f.store);
    let g = f.store.lock_recovered(&f.journal).unwrap();
    assert!(app.digest_groups(&g).unwrap().is_empty());
}

#[test]
fn app_import_reports_digests_whose_group_is_not_imported_and_keeps_group_times() {
    let f = import_fixture();
    let file = f.data().join("device.snbak");
    let mut entries = backup_entries();
    for (n, b) in &mut entries {
        if n.ends_with("knowledge.json") {
            *b = json_bytes(json!([
                {"serviceId": 11, "content": "in g1", "sourcePath": "Note/a.note",
                 "knowledgeBaseUniqueAttribute": "g1"},
                {"serviceId": 12, "content": "in g9", "sourcePath": "Note/b.note",
                 "knowledgeBaseUniqueAttribute": "g9"},
                {"serviceId": 13, "content": "no group", "sourcePath": "Note/c.note",
                 "knowledgeBaseUniqueAttribute": ""},
                {"serviceId": 14, "content": "no key", "sourcePath": "Note/d.note"}
            ]));
        }
        if n.ends_with("knowledge_bases.json") {
            *b = json_bytes(json!([
                {"serviceId": 20, "uniqueIdentifier": "g1", "name": "Ideas", "md5Hash": "gm",
                 "creationTime": 1_700_000_000_000i64, "createTime": 1_700_000_000_100i64,
                 "updateTime": "not a number", "userId": "x"}
            ]));
        }
    }
    snbak(&file, &entries);
    let dry =
        notedown_cloud::appdata::dry_run_app_data(f.data(), "jordi", Some(&file), false).unwrap();
    assert_eq!(dry.without_group, vec!["12  Note/b.note  group g9"]);
    let r = run(&f, Some(&file), &no_options()).unwrap();
    assert_eq!(r.without_group, dry.without_group);
    assert_eq!(r.digests, 4);
    let app = AppData::new(&f.store);
    let g = f.store.lock_recovered(&f.journal).unwrap();
    let grp = app.digest_group(&g, 20).unwrap().unwrap();
    // The backup's createTime wins over creationTime; updateTime is the import time.
    assert_eq!(grp.create_time, 1_700_000_000_100);
    assert_eq!(grp.update_time, T0 as u64);
    assert!(!grp.extra.contains_key("userId"), "{:?}", grp.extra);
}
