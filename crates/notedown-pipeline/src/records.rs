//! The page records of an account (`accounts/<slug>/pages/<key>.json`) and its note files,
//! read straight from disk.
//!
//! The pipeline never writes below `accounts/`, so it does not open a `notedown_cloud` `Store`
//! (opening one probes the folder with a temp file and compacts `ids.jsonl`). The checks are
//! those of `notedown_cloud::pages::read_record`: the key comes from the file name, and a
//! record whose `fileId` is not that key, or whose `noteId` is not a valid `FILE_ID`, is not
//! used.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use notedown_cloud::auth::Accounts;
use notedown_cloud::pages::{PAGES_DIR, PageRecord, valid_file_id, valid_key};
use notedown_cloud::store::WirePath;

use crate::{Error, Result};

/// A page record and the time of the note's last page change: the record file's mtime, in
/// ms since the epoch. The server writes a record only when the note's bytes change (and when
/// it moves the note).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub record: PageRecord,
    pub changed_ms: u64,
}

/// The page records of one account.
#[derive(Debug, Default)]
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

/// `accounts/<slug>`.
pub fn account_dir(data: &Path, slug: &str) -> PathBuf {
    Accounts::new(data).account_dir(slug)
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

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Reads every page record of account `slug`.
pub fn read(data: &Path, slug: &str) -> Result<Records> {
    let dir = account_dir(data, slug).join(PAGES_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Records::default()),
        Err(e) => return Err(io(&dir)(e)),
    };
    let mut out = Records {
        complete: true,
        ..Records::default()
    };
    for entry in entries {
        let entry = entry.map_err(io(&dir))?;
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

/// The record of `key`, `None` when it is missing or cannot be used.
pub fn read_key(data: &Path, slug: &str, key: &str) -> Option<Record> {
    if !valid_key(key) {
        return None;
    }
    let name = match key.strip_prefix("path:") {
        Some(md5) => format!("path-{md5}.json"),
        None => format!("{key}.json"),
    };
    read_one(&account_dir(data, slug).join(PAGES_DIR).join(name), key)
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
pub fn note_file(data: &Path, slug: &str, path: &str) -> Result<PathBuf> {
    let wire = WirePath::parse(path)?;
    Ok(wire.to_fs(&account_dir(data, slug).join("files")))
}

/// The page-index key of a link target. `file_id` is the raw `FILE_ID` of the target as the
/// note stores it, `target` its device path. The key is the `FILE_ID` when a record has it as
/// its key, else the key of the record whose `noteId` is that `FILE_ID` (a copy keyed by its
/// path), else the key of the record whose wire path ends the device path (a note without a
/// usable `FILE_ID`). `None` when no record matches.
pub fn link_key(
    records: &BTreeMap<String, Record>,
    file_id: Option<&str>,
    target: &str,
) -> Option<String> {
    if let Some(id) = file_id.filter(|id| !id.is_empty()) {
        if records.contains_key(id) {
            return Some(id.to_string());
        }
        if let Some((key, _)) = records
            .iter()
            .find(|(_, r)| r.record.note_id.as_deref() == Some(id))
        {
            return Some(key.clone());
        }
    }
    // The device path is like `/storage/emulated/0/Note/a.note`, the wire path `NOTE/Note/a.note`.
    let target = target.trim_end_matches('/');
    records
        .iter()
        .find(|(_, r)| {
            r.record
                .path
                .split_once('/')
                .is_some_and(|(_, rest)| !rest.is_empty() && target.ends_with(&format!("/{rest}")))
        })
        .map(|(key, _)| key.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(key: &str, note_id: Option<&str>, path: &str) -> (String, Record) {
        (
            key.to_string(),
            Record {
                record: PageRecord {
                    file_id: key.to_string(),
                    note_id: note_id.map(str::to_string),
                    path: path.to_string(),
                    size: 0,
                    md5: String::new(),
                    pages: Vec::new(),
                },
                changed_ms: 0,
            },
        )
    }

    #[test]
    fn a_link_resolves_to_the_page_index_key() {
        let copy = "path:0123456789abcdef0123456789abcdef";
        let plain = "path:fedcba9876543210fedcba9876543210";
        let records: BTreeMap<String, Record> = [
            rec("F1", Some("F1"), "NOTE/Note/a.note"),
            rec(copy, Some("F2"), "NOTE/Note/b.note"),
            rec(plain, None, "NOTE/Note/old/c.note"),
        ]
        .into_iter()
        .collect();
        assert_eq!(link_key(&records, Some("F1"), "x").as_deref(), Some("F1"));
        assert_eq!(link_key(&records, Some("F2"), "x").as_deref(), Some(copy));
        assert_eq!(
            link_key(&records, None, "/storage/emulated/0/Note/old/c.note").as_deref(),
            Some(plain)
        );
        assert_eq!(
            link_key(&records, Some("F9"), "/storage/emulated/0/Note/old/c.note").as_deref(),
            Some(plain)
        );
        assert_eq!(link_key(&records, Some("F9"), "/x/y.note"), None);
        assert_eq!(
            key_of("path-0123456789abcdef0123456789abcdef.json").as_deref(),
            Some(copy)
        );
        assert_eq!(key_of("F1.json").as_deref(), Some("F1"));
        assert_eq!(key_of("../x.json"), None);
    }

    #[test]
    fn records_are_read_with_their_checks() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("accounts/jordi/pages");
        let missing = read(tmp.path(), "jordi").unwrap();
        assert!(!missing.complete);
        assert!(!missing.gone("F1"));
        std::fs::create_dir_all(&dir).unwrap();
        let good = rec("F1", None, "NOTE/Note/a.note").1.record;
        std::fs::write(dir.join("F1.json"), serde_json::to_vec(&good).unwrap()).unwrap();
        let mut wrong = good.clone();
        wrong.file_id = "F3".into();
        std::fs::write(dir.join("F2.json"), serde_json::to_vec(&wrong).unwrap()).unwrap();
        std::fs::write(dir.join("F4.json"), "{").unwrap();
        let r = read(tmp.path(), "jordi").unwrap();
        assert!(r.complete);
        assert_eq!(r.records["F1"].record, good);
        assert!(r.records["F1"].changed_ms > 0);
        assert_eq!(r.unreadable.iter().collect::<Vec<_>>(), ["F2", "F4"]);
        assert!(!r.gone("F2") && !r.gone("F1") && r.gone("F5"));
        assert_eq!(read_key(tmp.path(), "jordi", "F1").unwrap().record, good);
        assert_eq!(
            note_file(tmp.path(), "jordi", "NOTE/Note/a.note").unwrap(),
            tmp.path().join("accounts/jordi/files/NOTE/Note/a.note")
        );
    }
}
