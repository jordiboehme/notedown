//! The server's data as the OCR pipeline sees it, read straight from the data folder: the
//! accounts' page records, note files and digests, and the journal. The pipeline's folder
//! source reads a (mounted) data folder with these functions, and the REST read API serves the
//! very same reads, so the results never depend on the transport.
//!
//! Nothing here writes, not even a lock file, so a read-only folder works. The readers do not
//! open a [`crate::store::Store`] (opening one probes the folder with a temp file and compacts
//! `ids.jsonl`). The record checks are those of [`crate::pages::read_record`]: the key comes
//! from the file name, and a record whose `fileId` is not that key, or whose `noteId` is not a
//! valid `FILE_ID`, is not used.

mod journal;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::appdata::{DIGESTS_DIR, Digest};
use crate::auth::Accounts;
use crate::pages::{PAGES_DIR, PageRecord, valid_file_id, valid_key};
use crate::store::WirePath;
use crate::{Result, io_err};

pub use journal::{journal, journal_until};

/// An account of the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerAccount {
    pub slug: String,
    pub user_id: u64,
}

/// A page record and the time of the note's last page change: the record file's mtime, in
/// ms since the epoch. The server writes a record only when the note's bytes change (and when
/// it moves the note).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub record: PageRecord,
    pub changed_ms: u64,
}

/// The page records of one account.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Records {
    /// By key.
    pub records: BTreeMap<String, Record>,
    /// Keys whose record file exists but cannot be used now. Their notes are left alone,
    /// never taken as deleted.
    pub unreadable: BTreeSet<String>,
    /// False when `pages/` itself is missing: nothing is known, so no note counts as deleted.
    pub complete: bool,
}

impl Records {
    /// Whether the note `key` may count as deleted: the folder was listed and holds no record
    /// file of it.
    pub fn gone(&self, key: &str) -> bool {
        self.complete && !self.records.contains_key(key) && !self.unreadable.contains(key)
    }
}

/// The bytes of a note, or the certain news that it is not there or not a note file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteBytes {
    Found(Vec<u8>),
    Missing,
    /// Something is at the note's path, but it is not a file (a folder, say). It stays so
    /// until someone changes it, so the worker counts it as a failure of the note.
    NotAFile,
}

/// A note file opened for reading, or the certain news that it is not there or not a note
/// file (as for [`NoteBytes`]).
#[derive(Debug)]
pub enum NoteFile {
    /// The open file and its size in bytes.
    Found(std::fs::File, u64),
    Missing,
    NotAFile,
}

/// An `account.removed` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Removal {
    pub seq: u64,
    pub user_id: u64,
}

/// One page of an account's journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalBatch {
    /// Any new line for this account, also lines that did not parse.
    pub changed: bool,
    /// The `account.removed` events of this batch, in `seq` order.
    pub removed: Vec<Removal>,
    /// One past the last `seq` read; the cursor may move to `next - 1`.
    pub next: u64,
    /// The highest `seq` in the journal as far as the source can tell, 0 if none.
    pub end: u64,
    /// A page limit was hit; ask again before sleeping.
    pub more: bool,
}

/// `accounts/<slug>`.
fn account_dir(root: &Path, slug: &str) -> PathBuf {
    Accounts::new(root).account_dir(slug)
}

/// The key of a record file name: `<FILE_ID>.json` or `path-<md5>.json`.
fn key_of(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".json")?;
    let key = match stem.strip_prefix("path-") {
        Some(md5) => format!("path:{md5}"),
        None => stem.to_string(),
    };
    valid_key(&key).then_some(key)
}

/// Every page record of account `slug`. A missing `pages/` (also of an unknown account) is
/// [`Records::default`], which is not `complete`.
pub fn records(root: &Path, slug: &str) -> Result<Records> {
    let dir = account_dir(root, slug).join(PAGES_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Records::default()),
        Err(e) => return Err(io_err(&dir)(e)),
    };
    let mut out = Records {
        complete: true,
        ..Records::default()
    };
    for entry in entries {
        let entry = entry.map_err(io_err(&dir))?;
        let Some(key) = entry.file_name().to_str().and_then(key_of) else {
            continue;
        };
        match read_one(&entry.path(), &key) {
            Some(r) => {
                out.records.insert(key, r);
            }
            None => {
                out.unreadable.insert(key);
            }
        }
    }
    Ok(out)
}

/// The record of `key`, `Ok(None)` when it is missing or cannot be used.
pub fn record(root: &Path, slug: &str, key: &str) -> Result<Option<Record>> {
    if !valid_key(key) {
        return Ok(None);
    }
    let name = match key.strip_prefix("path:") {
        Some(md5) => format!("path-{md5}.json"),
        None => format!("{key}.json"),
    };
    Ok(read_one(
        &account_dir(root, slug).join(PAGES_DIR).join(name),
        key,
    ))
}

fn read_one(path: &Path, key: &str) -> Option<Record> {
    let file = std::fs::File::open(path).ok()?;
    let changed_ms = file
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64);
    let record: PageRecord = match serde_json::from_reader(std::io::BufReader::new(file)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("warning: {}: cannot be read: {e}", path.display());
            return None;
        }
    };
    let ok = record.file_id == key
        && record
            .note_id
            .iter()
            .all(|id| valid_file_id(Some(id)).is_some());
    if !ok {
        eprintln!(
            "warning: {} has a fileId or noteId that does not match its name",
            path.display()
        );
        return None;
    }
    Some(Record { record, changed_ms })
}

/// The file of the note at wire `path`: `accounts/<slug>/files/<path>`.
fn note_file(root: &Path, slug: &str, path: &str) -> Result<PathBuf> {
    let wire = WirePath::parse(path)?;
    Ok(wire.to_fs(&account_dir(root, slug).join("files")))
}

/// Opens the note at wire `path`. `Missing` only when the account's `files/` exists and the
/// note does not; `NotAFile` when something that is not a file is at its path; a missing
/// account is `Err`.
pub fn open_note(root: &Path, slug: &str, path: &str) -> Result<NoteFile> {
    let file = note_file(root, slug, path)?;
    if std::fs::metadata(&file).is_ok_and(|m| !m.is_file()) {
        return Ok(NoteFile::NotAFile);
    }
    match std::fs::File::open(&file) {
        Ok(f) => {
            // Opening a folder works on Unix, so the open handle says what it is.
            let meta = f.metadata().map_err(io_err(&file))?;
            if !meta.is_file() {
                return Ok(NoteFile::NotAFile);
            }
            Ok(NoteFile::Found(f, meta.len()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if account_dir(root, slug).join("files").is_dir() {
                Ok(NoteFile::Missing)
            } else {
                Err(io_err(&file)(e))
            }
        }
        Err(e) => Err(io_err(&file)(e)),
    }
}

/// The bytes of the note at wire `path`, as [`open_note`] finds it.
pub fn note(root: &Path, slug: &str, path: &str) -> Result<NoteBytes> {
    match open_note(root, slug, path)? {
        NoteFile::Found(mut f, size) => {
            let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
            f.read_to_end(&mut bytes)
                .map_err(io_err(&note_file(root, slug, path)?))?;
            Ok(NoteBytes::Found(bytes))
        }
        NoteFile::Missing => Ok(NoteBytes::Missing),
        NoteFile::NotAFile => Ok(NoteBytes::NotAFile),
    }
}

/// A cheap check without the bytes: `Some(false)` as for [`NoteBytes::Missing`], `Some(true)`
/// for a file, `None` when nothing is known.
pub fn note_exists(root: &Path, slug: &str, path: &str) -> Result<Option<bool>> {
    let file = note_file(root, slug, path)?;
    if note_missing(root, slug, path) {
        return Ok(Some(false));
    }
    Ok(file.is_file().then_some(true))
}

/// Whether the note file at wire `path` is missing: it is not there, and the account's
/// `files/` folder is. Any other error (a folder in its place, no access) is no answer, and
/// without `files/` nothing is known, so neither counts as missing.
fn note_missing(root: &Path, slug: &str, path: &str) -> bool {
    let Ok(file) = note_file(root, slug, path) else {
        return false;
    };
    matches!(std::fs::metadata(&file), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
        && account_dir(root, slug).join("files").is_dir()
}

/// The live digest items of account `slug`, read from `accounts/<slug>/digests/items/*.json`
/// (spec 2.1), sorted by id. A file that cannot be read is left out with a warning.
pub fn digests(root: &Path, slug: &str) -> Result<Vec<Digest>> {
    let dir = account_dir(root, slug).join(DIGESTS_DIR).join("items");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(&dir)(e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let path = entry.map_err(io_err(&dir))?.path();
        let is_json = path.extension().is_some_and(|x| x == "json")
            && !path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'));
        if !is_json {
            continue;
        }
        let digest = std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice::<Digest>(&b).map_err(|e| e.to_string()));
        match digest {
            Ok(d) if d.is_live() => out.push(d),
            Ok(_) => {}
            Err(e) => eprintln!("warning: {}: cannot be read: {e}", path.display()),
        }
    }
    out.sort_by_key(|d| d.id);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(key: &str, path: &str) -> PageRecord {
        PageRecord {
            file_id: key.to_string(),
            note_id: None,
            path: path.to_string(),
            size: 0,
            md5: String::new(),
            pages: Vec::new(),
        }
    }

    #[test]
    fn record_file_names_give_their_keys() {
        assert_eq!(
            key_of("path-0123456789abcdef0123456789abcdef.json").as_deref(),
            Some("path:0123456789abcdef0123456789abcdef")
        );
        assert_eq!(key_of("F1.json").as_deref(), Some("F1"));
        assert_eq!(key_of("../x.json"), None);
    }

    #[test]
    fn records_are_read_with_their_checks() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("accounts/jordi/pages");
        let missing = records(tmp.path(), "jordi").unwrap();
        assert!(!missing.complete);
        assert!(!missing.gone("F1"));
        std::fs::create_dir_all(&dir).unwrap();
        let good = rec("F1", "NOTE/Note/a.note");
        std::fs::write(dir.join("F1.json"), serde_json::to_vec(&good).unwrap()).unwrap();
        let mut wrong = good.clone();
        wrong.file_id = "F3".into();
        std::fs::write(dir.join("F2.json"), serde_json::to_vec(&wrong).unwrap()).unwrap();
        std::fs::write(dir.join("F4.json"), "{").unwrap();
        let r = records(tmp.path(), "jordi").unwrap();
        assert!(r.complete);
        assert_eq!(r.records["F1"].record, good);
        assert!(r.records["F1"].changed_ms > 0);
        assert_eq!(r.unreadable.iter().collect::<Vec<_>>(), ["F2", "F4"]);
        assert!(!r.gone("F2") && !r.gone("F1") && r.gone("F5"));
        assert_eq!(
            record(tmp.path(), "jordi", "F1").unwrap().unwrap().record,
            good
        );
        assert_eq!(record(tmp.path(), "jordi", "F2").unwrap(), None);
        assert_eq!(record(tmp.path(), "jordi", "../F1").unwrap(), None);
        assert_eq!(
            note_file(tmp.path(), "jordi", "NOTE/Note/a.note").unwrap(),
            tmp.path().join("accounts/jordi/files/NOTE/Note/a.note")
        );
    }

    #[test]
    fn records_travel_as_camel_case_json() {
        let mut r = Records {
            complete: true,
            ..Records::default()
        };
        r.records.insert(
            "F1".into(),
            Record {
                record: rec("F1", "NOTE/Note/a.note"),
                changed_ms: 7,
            },
        );
        r.unreadable.insert("F2".into());
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["records"]["F1"]["changedMs"], 7);
        let back: Records = serde_json::from_value(json).unwrap();
        assert_eq!(back.records, r.records);
        assert_eq!(back.unreadable, r.unreadable);
        assert!(back.complete);
        let b = JournalBatch {
            changed: true,
            removed: vec![Removal { seq: 3, user_id: 9 }],
            next: 4,
            end: 3,
            more: false,
        };
        let json = serde_json::to_value(&b).unwrap();
        assert_eq!(json["removed"][0]["userId"], 9);
        assert_eq!(serde_json::from_value::<JournalBatch>(json).unwrap(), b);
        let a = ServerAccount {
            slug: "jordi".into(),
            user_id: 5,
        };
        assert_eq!(serde_json::to_value(&a).unwrap()["userId"], 5);
    }

    #[test]
    fn notes_are_found_missing_or_not_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // No account folder: nothing known.
        assert!(open_note(root, "jordi", "NOTE/Note/a.note").is_err());
        assert!(note(root, "jordi", "NOTE/Note/a.note").is_err());
        assert_eq!(
            note_exists(root, "jordi", "NOTE/Note/a.note").unwrap(),
            None
        );
        let files = root.join("accounts/jordi/files/NOTE/Note");
        std::fs::create_dir_all(&files).unwrap();
        std::fs::write(files.join("a.note"), b"abc").unwrap();
        std::fs::create_dir(files.join("dir.note")).unwrap();
        match open_note(root, "jordi", "NOTE/Note/a.note").unwrap() {
            NoteFile::Found(mut f, size) => {
                assert_eq!(size, 3);
                let mut s = String::new();
                f.read_to_string(&mut s).unwrap();
                assert_eq!(s, "abc");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            note(root, "jordi", "NOTE/Note/a.note").unwrap(),
            NoteBytes::Found(b"abc".to_vec())
        );
        assert!(matches!(
            open_note(root, "jordi", "NOTE/Note/dir.note").unwrap(),
            NoteFile::NotAFile
        ));
        assert_eq!(
            note(root, "jordi", "NOTE/Note/dir.note").unwrap(),
            NoteBytes::NotAFile
        );
        assert!(matches!(
            open_note(root, "jordi", "NOTE/Note/none.note").unwrap(),
            NoteFile::Missing
        ));
        assert_eq!(
            note(root, "jordi", "NOTE/Note/none.note").unwrap(),
            NoteBytes::Missing
        );
        assert_eq!(
            note_exists(root, "jordi", "NOTE/Note/a.note").unwrap(),
            Some(true)
        );
        assert_eq!(
            note_exists(root, "jordi", "NOTE/Note/none.note").unwrap(),
            Some(false)
        );
        assert_eq!(
            note_exists(root, "jordi", "NOTE/Note/dir.note").unwrap(),
            None
        );
        assert!(open_note(root, "jordi", "../x").is_err());
    }

    #[test]
    fn only_live_digests_are_read() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(digests(tmp.path(), "jordi").unwrap().is_empty());
        let dir = tmp.path().join("accounts/jordi/digests/items");
        std::fs::create_dir_all(&dir).unwrap();
        for (id, deleted) in [(3u64, "N"), (1, "N"), (2, "Y")] {
            let d = Digest {
                id,
                is_deleted: deleted.to_string(),
                ..Digest::default()
            };
            std::fs::write(
                dir.join(format!("{id}.json")),
                serde_json::to_vec(&d).unwrap(),
            )
            .unwrap();
        }
        std::fs::write(dir.join("5.json"), "{").unwrap();
        std::fs::write(dir.join(".6.json"), "{").unwrap();
        let ids: Vec<u64> = digests(tmp.path(), "jordi")
            .unwrap()
            .iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(ids, [1, 3]);
    }
}
