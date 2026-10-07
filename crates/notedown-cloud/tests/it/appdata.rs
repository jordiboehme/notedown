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
