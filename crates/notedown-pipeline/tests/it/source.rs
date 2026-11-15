//! The `ServerData` contract. The functions take any source, so the REST source of plan 2 runs
//! the same checks as the folder source.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

use notedown_cloud::auth::Accounts;
use notedown_cloud::journal::Journal;
use notedown_pipeline::Error;
use notedown_pipeline::records::{Record, Records};
use notedown_pipeline::source::{
    FolderSource, JournalBatch, NoteBytes, Removal, ServerAccount, ServerData,
};

use crate::worker::{Data, SLUG, setup};

/// A folder source that fails every call while `fail` is set, like a server that is away.
pub(crate) struct Flaky {
    pub inner: FolderSource,
    pub fail: Arc<AtomicBool>,
}

impl Flaky {
    fn check(&self) -> notedown_pipeline::Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            Err(Error::Read("the server is away".into()))
        } else {
            Ok(())
        }
    }
}

impl ServerData for Flaky {
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn accounts(&self) -> notedown_pipeline::Result<Vec<ServerAccount>> {
        self.check()?;
        self.inner.accounts()
    }
    fn records(&self, slug: &str) -> notedown_pipeline::Result<Records> {
        self.check()?;
        self.inner.records(slug)
    }
    fn record(&self, slug: &str, key: &str) -> notedown_pipeline::Result<Option<Record>> {
        self.check()?;
        self.inner.record(slug, key)
    }
    fn note(&self, slug: &str, path: &str) -> notedown_pipeline::Result<NoteBytes> {
        self.check()?;
        self.inner.note(slug, path)
    }
    fn note_exists(&self, slug: &str, path: &str) -> notedown_pipeline::Result<Option<bool>> {
        self.check()?;
        self.inner.note_exists(slug, path)
    }
    fn digests(
        &self,
        slug: &str,
    ) -> notedown_pipeline::Result<Vec<notedown_cloud::appdata::Digest>> {
        self.check()?;
        self.inner.digests(slug)
    }
    fn journal(
        &self,
        slug: &str,
        from: u64,
        limit: usize,
    ) -> notedown_pipeline::Result<JournalBatch> {
        self.check()?;
        self.inner.journal(slug, from, limit)
    }
}

fn mtime_ms(path: PathBuf) -> u64 {
    std::fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/notes")
        .join(format!("{name}.note"))
}

pub(crate) fn journal_from_a_position(src: &dyn ServerData, d: &Data) {
    d.put("test1", "NOTE/Note");
    d.put("test2", "NOTE/Note");
    let all = src.journal(SLUG, 1, 10_000).unwrap();
    assert!(all.changed);
    assert!(!all.more);
    assert!(all.next > 1);
    assert_eq!(all.end, all.next - 1);
    let again = src.journal(SLUG, all.next, 10_000).unwrap();
    assert!(!again.changed);
    assert_eq!(again.next, all.next);
    assert_eq!(again.end, all.end);
    assert!(again.removed.is_empty());
    let one = src.journal(SLUG, 1, 1).unwrap();
    assert!(one.more);
    assert!(one.changed);
    assert_eq!(one.next, 2);
    assert_eq!(one.end, all.end);
    // A position past the journal does not raise its end: the caller must see it is ahead.
    let ahead = src.journal(SLUG, all.end + 100, 10_000).unwrap();
    assert_eq!(ahead.end, all.end);
    assert_eq!(ahead.next, all.end + 100);
    assert!(!ahead.changed);
}

pub(crate) fn journal_is_per_account(src: &dyn ServerData, d: &Data) {
    d.put("test1", "NOTE/Note");
    let before = src.journal(SLUG, 1, 10_000).unwrap();
    Accounts::new(&d.data)
        .create("anna", "anna@example.org", "secret")
        .unwrap();
    let journal = Journal::open(&d.data).unwrap();
    notedown_cloud::put::put(
        &d.data,
        "anna",
        &journal,
        &fixture("test2"),
        "NOTE/Note",
        None,
    )
    .unwrap();
    let after = src.journal(SLUG, before.next, 10_000).unwrap();
    assert!(!after.changed);
    assert!(after.removed.is_empty());
    assert!(after.next > before.next);
    assert_eq!(after.end, after.next - 1);
    let anna = src.journal("anna", before.next, 10_000).unwrap();
    assert!(anna.changed);
}

pub(crate) fn records_with_change_times(src: &dyn ServerData, d: &Data) {
    let key = d.put("test1", "NOTE/Note");
    let r = src.records(SLUG).unwrap();
    assert!(r.complete);
    assert_eq!(r.records[&key].changed_ms, mtime_ms(d.record_path(&key)));
    let one = src.record(SLUG, &key).unwrap().unwrap();
    assert_eq!(one, r.records[&key]);
    assert!(src.record(SLUG, "NOSUCHKEY").unwrap().is_none());
}

pub(crate) fn note_bytes(src: &dyn ServerData, d: &Data) {
    d.put("test1", "NOTE/Note");
    let want = std::fs::read(fixture("test1")).unwrap();
    match src.note(SLUG, "NOTE/Note/test1.note").unwrap() {
        NoteBytes::Found(b) => assert_eq!(b, want),
        NoteBytes::Missing => panic!("the note is there"),
    }
    assert!(matches!(
        src.note(SLUG, "NOTE/Note/nothing.note").unwrap(),
        NoteBytes::Missing
    ));
    assert_eq!(
        src.note_exists(SLUG, "NOTE/Note/test1.note").unwrap(),
        Some(true)
    );
    assert_eq!(
        src.note_exists(SLUG, "NOTE/Note/nothing.note").unwrap(),
        Some(false)
    );
    assert!(src.note("nobody", "NOTE/Note/test1.note").is_err());
    assert!(!matches!(
        src.note_exists("nobody", "NOTE/Note/test1.note"),
        Ok(Some(false))
    ));
}

pub(crate) fn digests_keep_only_live_ones(src: &dyn ServerData, d: &Data) {
    let items = d.data.join("accounts").join(SLUG).join("digests/items");
    std::fs::create_dir_all(&items).unwrap();
    for (id, deleted) in [(11u64, "N"), (12, "Y")] {
        let v = serde_json::json!({
            "id": id, "sourceType": 2, "isDeleted": deleted, "isSummaryGroup": "N",
        });
        std::fs::write(items.join(format!("{id}.json")), v.to_string()).unwrap();
    }
    let ids: Vec<u64> = src.digests(SLUG).unwrap().iter().map(|x| x.id).collect();
    assert_eq!(ids, [11]);
}

pub(crate) fn a_missing_account(src: &dyn ServerData, d: &Data) {
    d.put("test1", "NOTE/Note");
    if let Ok(r) = src.records("nobody") {
        assert!(!r.complete);
        assert!(!r.gone("anything"));
    }
}

pub(crate) fn removed_accounts(src: &dyn ServerData, d: &Data) {
    let accounts = Accounts::new(&d.data);
    let bob = accounts.create("bob", "bob@example.org", "secret").unwrap();
    let journal = Journal::open(&d.data).unwrap();
    notedown_cloud::put::put(
        &d.data,
        "bob",
        &journal,
        &fixture("test2"),
        "NOTE/Note",
        None,
    )
    .unwrap();
    let from = src.journal("bob", 1, 10_000).unwrap().next;
    let other_from = src.journal(SLUG, 1, 10_000).unwrap().next;
    accounts.remove("bob", &journal).unwrap();
    let b = src.journal("bob", from, 10_000).unwrap();
    assert!(b.changed);
    assert_eq!(
        b.removed,
        [Removal {
            seq: b.end,
            user_id: bob.user_id
        }]
    );
    let other = src.journal(SLUG, other_from, 10_000).unwrap();
    assert!(other.removed.is_empty());
}

fn contract(src: &dyn ServerData, d: &Data) {
    journal_from_a_position(src, d);
    journal_is_per_account(src, d);
    records_with_change_times(src, d);
    note_bytes(src, d);
    digests_keep_only_live_ones(src, d);
    a_missing_account(src, d);
    removed_accounts(src, d);
}

#[test]
fn folder_source_meets_the_contract() {
    let d = setup();
    let src = FolderSource::new(&d.data);
    contract(&src, &d);
    let accounts = src.accounts().unwrap();
    assert!(accounts.iter().any(|a| a.slug == SLUG && a.user_id > 0));
    assert!(src.describe().contains(d.data.to_str().unwrap()));
}

#[test]
fn a_missing_server_folder_is_nothing_known() {
    let dir = tempfile::tempdir().unwrap();
    let src = FolderSource::new(&dir.path().join("missing"));
    assert!(src.accounts().map(|a| a.is_empty()).unwrap_or(true));
    assert!(src.records(SLUG).map(|r| !r.complete).unwrap_or(true));
    let b = src.journal(SLUG, 1, 10_000).unwrap();
    assert!(!b.changed);
    assert!(b.removed.is_empty());
    assert_eq!(b.end, 0);
    assert_eq!(b.next, 1);
    assert!(!b.more);
}

/// Every path, its kind, size, modification time and bytes.
fn tree(root: &Path) -> Vec<(PathBuf, bool, u64, u64, Vec<u8>)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(PathBuf, bool, u64, u64, Vec<u8>)>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap())
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            let m = e.metadata().unwrap();
            let rel = p.strip_prefix(root).unwrap().to_path_buf();
            let bytes = if m.is_file() {
                std::fs::read(&p).unwrap()
            } else {
                vec![]
            };
            out.push((rel, m.is_dir(), m.len(), mtime_ms(p.clone()), bytes));
            if m.is_dir() {
                walk(&p, root, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

#[test]
fn the_folder_source_writes_nothing() {
    let d = setup();
    d.put("test1", "NOTE/Note");
    let src = FolderSource::new(&d.data);
    let before = tree(&d.data);
    let _ = src.accounts().unwrap();
    let _ = src.journal(SLUG, 1, 10_000).unwrap();
    let _ = src.records(SLUG).unwrap();
    let _ = src.note(SLUG, "NOTE/Note/test1.note").unwrap();
    let _ = src.digests(SLUG).unwrap();
    assert_eq!(tree(&d.data), before);
    // And on a folder without a journal folder, nothing appears.
    let empty = tempfile::tempdir().unwrap();
    let src = FolderSource::new(empty.path());
    let _ = src.journal(SLUG, 1, 10).unwrap();
    assert_eq!(std::fs::read_dir(empty.path()).unwrap().count(), 0);
}

#[test]
fn a_flaky_source_is_err_while_it_fails() {
    let d = setup();
    d.put("test1", "NOTE/Note");
    let fail = Arc::new(AtomicBool::new(true));
    let src = Flaky {
        inner: FolderSource::new(&d.data),
        fail: fail.clone(),
    };
    assert!(src.accounts().is_err());
    assert!(src.records(SLUG).is_err());
    assert!(src.record(SLUG, "K").is_err());
    assert!(src.note(SLUG, "NOTE/Note/test1.note").is_err());
    assert!(src.note_exists(SLUG, "NOTE/Note/test1.note").is_err());
    assert!(src.digests(SLUG).is_err());
    assert!(src.journal(SLUG, 1, 10).is_err());
    fail.store(false, Ordering::SeqCst);
    assert!(src.records(SLUG).unwrap().complete);
}

#[test]
fn a_line_that_does_not_parse_counts_as_changed() {
    let d = setup();
    d.put("test1", "NOTE/Note");
    let src = FolderSource::new(&d.data);
    let end = src.journal(SLUG, 1, 10_000).unwrap().end;
    let file = Journal::open(&d.data)
        .unwrap()
        .files()
        .unwrap()
        .pop()
        .unwrap()
        .1;
    let mut text = std::fs::read_to_string(&file).unwrap();
    // Not JSON at all: counts for every account. JSON of another account: only for that one.
    text.push_str("garbage\n");
    text.push_str(&format!(
        "{{\"seq\":{},\"account\":\"anna\",\"type\":\"future.thing\"}}\n",
        end + 2
    ));
    std::fs::write(&file, text).unwrap();
    let b = src.journal(SLUG, end + 1, 10_000).unwrap();
    assert!(b.changed);
    assert_eq!(b.next, end + 3);
    let only_anna = src.journal(SLUG, end + 2, 10_000).unwrap();
    assert!(!only_anna.changed);
    assert!(src.journal("anna", end + 2, 10_000).unwrap().changed);
}

fn event_line(seq: u64, account: &str, user_id: u64) -> String {
    use notedown_cloud::journal::{Event, EventBody, Origin};
    let mut e = Event::new(
        account,
        Origin::Cli,
        None,
        "op",
        EventBody::AccountRemoved { user_id },
    );
    e.seq = seq;
    e.to_line()
}

fn write_journal(root: &Path, lines: &[String]) {
    let dir = root.join("journal");
    std::fs::create_dir_all(&dir).unwrap();
    let mut text = lines.join("\n");
    text.push('\n');
    std::fs::write(dir.join("journal-000000000001.jsonl"), text).unwrap();
}

#[test]
fn a_garbage_line_at_the_page_boundary_takes_no_slot() {
    let dir = tempfile::tempdir().unwrap();
    write_journal(
        dir.path(),
        &[
            event_line(1, "a", 1),
            "garbage".into(),
            event_line(2, "a", 2),
        ],
    );
    let src = FolderSource::new(dir.path());
    let b = src.journal("a", 2, 1).unwrap();
    assert!(b.changed);
    assert_eq!(b.next, 3);
    assert_eq!(b.removed, [Removal { seq: 2, user_id: 2 }]);
}

#[test]
fn a_trailing_garbage_line_does_not_skip_the_next_event() {
    let dir = tempfile::tempdir().unwrap();
    write_journal(dir.path(), &[event_line(1, "a", 1), "garbage".into()]);
    let src = FolderSource::new(dir.path());
    let b = src.journal("a", 2, 10).unwrap();
    assert!(b.changed);
    assert_eq!(b.next, 2);
    write_journal(dir.path(), &[event_line(1, "a", 1), event_line(2, "a", 2)]);
    let b = src.journal("a", b.next, 10).unwrap();
    assert_eq!(b.removed, [Removal { seq: 2, user_id: 2 }]);
}

#[test]
fn a_journal_folder_that_cannot_be_looked_at_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file");
    std::fs::write(&file, "x").unwrap();
    // `file/journal` fails with "not a directory", not "not found".
    assert!(FolderSource::new(&file).journal("a", 1, 10).is_err());
}

#[test]
fn a_limit_of_zero_still_makes_progress() {
    let dir = tempfile::tempdir().unwrap();
    write_journal(dir.path(), &[event_line(1, "a", 1), event_line(2, "a", 2)]);
    let b = FolderSource::new(dir.path()).journal("a", 1, 0).unwrap();
    assert_eq!(b.next, 2);
    assert!(b.more);
}
