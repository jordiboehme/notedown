//! The store on disk: round trip, a broken file, saves only on change, two writers.

use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Barrier};

use super::*;

const KEY: &str = "local|jordi";

fn full() -> NotifyRecord {
    let key = KeyRecord {
        last_sync_ms: Some(1_763_290_900_000),
        cursor: Some(813),
        last_count: Some(SyncCount {
            at_ms: 1_763_290_900_000,
            files: Some(12),
        }),
        count_failing_since: Some(1_763_291_000_000),
        sync_shown_at: Some(1_763_290_000_000),
        sync_last_notice_ms: Some(1_763_290_100_000),
        unshown_files: 3,
        ocr_text_seq: Some(4_000),
        ocr_data_dir: Some(PathBuf::from("/Users/me/notedown-data")),
        ocr_last_run_ms: Some(1_763_299_000_000),
        ocr_last_notice_ms: Some(1_763_298_000_000),
        unshown_notes: 2,
        consumers: [(
            "claude".to_owned(),
            ConsumerRecord {
                seq: 40,
                pending_seq: Some(44),
                rose_at: Some(1_763_299_500_000),
            },
        )]
        .into(),
        problems: [(
            "oldsync".to_owned(),
            ProblemState {
                active: true,
                last_notice_ms: 1_763_200_000_000,
                for_sync: Some(1_763_000_000_000),
            },
        )]
        .into(),
    };
    NotifyRecord {
        keys: [(KEY.to_owned(), key)].into(),
    }
}

#[test]
fn a_missing_file_is_an_empty_record() {
    let dir = tempfile::tempdir().unwrap();
    let store = NotifyStore::load(dir.path());
    assert_eq!(store.read(), NotifyRecord::default());
    assert!(!dir.path().join(FILE).exists());
}

#[test]
fn the_record_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let store = NotifyStore::load(dir.path());
    store.update(|r| *r = full()).unwrap();
    assert_eq!(store.read(), full());
    assert_eq!(NotifyStore::load(dir.path()).read(), full());
    assert!(!dir.path().join(TEMP).exists());
    // camelCase on disk, like the other files of the app.
    let text = fs::read_to_string(dir.path().join(FILE)).unwrap();
    for key in [
        "\"lastSyncMs\"",
        "\"lastCount\"",
        "\"atMs\"",
        "\"countFailingSince\"",
        "\"syncShownAt\"",
        "\"unshownFiles\"",
        "\"ocrTextSeq\"",
        "\"ocrDataDir\"",
        "\"pendingSeq\"",
        "\"roseAt\"",
        "\"lastNoticeMs\"",
        "\"forSync\"",
    ] {
        assert!(text.contains(key), "{key} in {text}");
    }
}

#[test]
fn missing_fields_load_as_their_defaults() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(FILE),
        r#"{"keys":{"local|jordi":{"lastSyncMs":5,"consumers":{"claude":{"seq":3}}}}}"#,
    )
    .unwrap();
    let record = NotifyStore::load(dir.path()).read();
    let key = &record.keys[KEY];
    assert_eq!(key.last_sync_ms, Some(5));
    assert_eq!(key.cursor, None);
    assert_eq!(key.unshown_files, 0);
    assert_eq!(
        key.consumers["claude"],
        ConsumerRecord {
            seq: 3,
            pending_seq: None,
            rose_at: None
        }
    );
    assert!(key.problems.is_empty());
    assert!(!dir.path().join(BROKEN).exists());
}

#[test]
fn a_broken_file_is_kept_aside_and_the_record_starts_empty() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(FILE), "{not json").unwrap();
    let store = NotifyStore::load(dir.path());
    assert_eq!(store.read(), NotifyRecord::default());
    assert_eq!(
        fs::read_to_string(dir.path().join(BROKEN)).unwrap(),
        "{not json"
    );
    assert!(!dir.path().join(FILE).exists());
    // The next change writes a good file; the broken one stays.
    store
        .update(|r| r.keys.entry(KEY.into()).or_default().cursor = Some(7))
        .unwrap();
    assert_eq!(
        NotifyStore::load(dir.path()).read().keys[KEY].cursor,
        Some(7)
    );
    assert!(dir.path().join(BROKEN).exists());
}

#[test]
fn update_saves_only_on_a_change() {
    let dir = tempfile::tempdir().unwrap();
    let store = NotifyStore::load(dir.path());
    // Nothing changed: no file.
    assert_eq!(store.update(|r| r.keys.len()).unwrap(), 0);
    assert!(!dir.path().join(FILE).exists());
    store.update(|r| *r = full()).unwrap();
    assert!(dir.path().join(FILE).exists());
    // Without a change the file is not written again.
    fs::remove_file(dir.path().join(FILE)).unwrap();
    let seq = store
        .update(|r| r.keys[KEY].consumers["claude"].seq)
        .unwrap();
    assert_eq!(seq, 40);
    store
        .update(|r| r.keys.get_mut(KEY).unwrap().unshown_files = 3)
        .unwrap();
    assert!(!dir.path().join(FILE).exists());
    // A change writes the whole record.
    store
        .update(|r| r.keys.get_mut(KEY).unwrap().unshown_files = 4)
        .unwrap();
    let mut want = full();
    want.keys.get_mut(KEY).unwrap().unshown_files = 4;
    assert_eq!(NotifyStore::load(dir.path()).read(), want);
}

/// Sets the mode of `path` and puts it back when dropped, so a failed test leaves no folder
/// the temp dir cannot remove.
struct Mode<'a>(&'a Path, u32);

impl<'a> Mode<'a> {
    fn set(path: &'a Path, mode: u32, back: u32) -> Self {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        Mode(path, back)
    }
}

impl Drop for Mode<'_> {
    fn drop(&mut self) {
        let _ = fs::set_permissions(self.0, fs::Permissions::from_mode(self.1));
    }
}

#[test]
fn a_failed_save_keeps_the_record_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let store = NotifyStore::load(dir.path());
    // A folder nothing can be written to.
    let mode = Mode::set(dir.path(), 0o555, 0o755);
    let result = store.update(|r| r.keys.entry(KEY.into()).or_default().cursor = Some(1));
    drop(mode);
    assert!(result.is_err());
    assert_eq!(store.read(), NotifyRecord::default());
    assert!(!dir.path().join(FILE).exists());
}

#[test]
fn a_login_without_fields_is_no_change() {
    let dir = tempfile::tempdir().unwrap();
    let store = NotifyStore::load(dir.path());
    store
        .update(|r| {
            r.keys.entry(KEY.into()).or_default();
        })
        .unwrap();
    assert!(!dir.path().join(FILE).exists());
    assert_eq!(store.read(), NotifyRecord::default());
    // Next to a saved record too.
    store.update(|r| *r = full()).unwrap();
    fs::remove_file(dir.path().join(FILE)).unwrap();
    store
        .update(|r| {
            r.keys.entry("another|key".into()).or_default();
        })
        .unwrap();
    assert!(!dir.path().join(FILE).exists());
    assert_eq!(store.read(), full());
}

#[test]
fn a_panic_in_the_change_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = NotifyStore::load(dir.path());
    store.update(|r| *r = full()).unwrap();
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = store.update(|r| {
            r.keys.get_mut(KEY).unwrap().unshown_files = 99;
            panic!("half way");
        });
    }));
    assert!(panicked.is_err());
    assert_eq!(store.read(), full());
    // The next change does not carry the half change along.
    store
        .update(|r| r.keys.get_mut(KEY).unwrap().cursor = Some(900))
        .unwrap();
    let mut want = full();
    want.keys.get_mut(KEY).unwrap().cursor = Some(900);
    assert_eq!(NotifyStore::load(dir.path()).read(), want);
}

#[test]
fn a_file_that_cannot_be_read_is_not_written_over() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(FILE);
    NotifyStore::load(dir.path())
        .update(|r| *r = full())
        .unwrap();
    let saved = fs::read(&path).unwrap();
    let mode = Mode::set(&path, 0o000, 0o644);
    let store = NotifyStore::load(dir.path());
    assert_eq!(store.read(), NotifyRecord::default());
    // Changes stay in memory; the file is left alone.
    store
        .update(|r| r.keys.entry("local|other".into()).or_default().cursor = Some(1))
        .unwrap();
    assert_eq!(store.read().keys["local|other"].cursor, Some(1));
    drop(mode);
    assert_eq!(fs::read(&path).unwrap(), saved);
    assert!(!dir.path().join(BROKEN).exists());
    // Once it reads again, its record is the one that changes and is saved.
    store
        .update(|r| r.keys.get_mut(KEY).unwrap().cursor = Some(900))
        .unwrap();
    let mut want = full();
    want.keys.get_mut(KEY).unwrap().cursor = Some(900);
    assert_eq!(store.read(), want);
    assert_eq!(NotifyStore::load(dir.path()).read(), want);
}

#[test]
fn a_broken_file_that_cannot_be_set_aside_is_not_written_over() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(FILE), "{not json").unwrap();
    // No rename in a read-only folder.
    let mode = Mode::set(dir.path(), 0o555, 0o755);
    let store = NotifyStore::load(dir.path());
    assert_eq!(store.read(), NotifyRecord::default());
    store
        .update(|r| r.keys.entry(KEY.into()).or_default().cursor = Some(7))
        .unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join(FILE)).unwrap(),
        "{not json"
    );
    assert!(!dir.path().join(BROKEN).exists());
    drop(mode);
    // Once it can be set aside, the record in memory is saved.
    store
        .update(|r| r.keys.get_mut(KEY).unwrap().unshown_files = 2)
        .unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join(BROKEN)).unwrap(),
        "{not json"
    );
    let key = &NotifyStore::load(dir.path()).read().keys[KEY];
    assert_eq!((key.cursor, key.unshown_files), (Some(7), 2));
}

#[test]
fn two_writers_keep_each_others_fields() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(NotifyStore::load(dir.path()));
    let barrier = Arc::new(Barrier::new(2));
    const ROUNDS: u64 = 50;
    let poller = {
        let (store, barrier) = (store.clone(), barrier.clone());
        std::thread::spawn(move || {
            barrier.wait();
            for i in 1..=ROUNDS {
                store
                    .update(|r| {
                        let key = r.keys.entry(KEY.into()).or_default();
                        key.last_sync_ms = Some(i);
                        key.cursor = Some(i * 10);
                    })
                    .unwrap();
            }
        })
    };
    let notifier = {
        let (store, barrier) = (store.clone(), barrier.clone());
        std::thread::spawn(move || {
            barrier.wait();
            for i in 1..=ROUNDS {
                store
                    .update(|r| {
                        let key = r.keys.entry(KEY.into()).or_default();
                        key.sync_shown_at = Some(i);
                        key.consumers.entry("claude".into()).or_default().seq = i;
                    })
                    .unwrap();
            }
        })
    };
    poller.join().unwrap();
    notifier.join().unwrap();
    // On disk, not only in memory: the last save holds both writers' last fields.
    let record = NotifyStore::load(dir.path()).read();
    let key = &record.keys[KEY];
    assert_eq!(key.last_sync_ms, Some(ROUNDS));
    assert_eq!(key.cursor, Some(ROUNDS * 10));
    assert_eq!(key.sync_shown_at, Some(ROUNDS));
    assert_eq!(key.consumers["claude"].seq, ROUNDS);
    assert_eq!(record, store.read());
}
