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

use notedown_cloud::appdata::{DIGESTS_DIR, Digest};
use notedown_cloud::auth::Accounts;
use notedown_cloud::pages::{PAGES_DIR, PageRecord, valid_file_id, valid_key};
use notedown_cloud::store::WirePath;
use notedown_note::DigestRef;

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

/// The live digest items of account `slug`, read from `accounts/<slug>/digests/items/*.json`
/// (spec 2.1). A file that cannot be read is left out with a warning.
pub fn digests(data: &Path, slug: &str) -> Result<Vec<Digest>> {
    let dir = account_dir(data, slug).join(DIGESTS_DIR).join("items");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io(&dir)(e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let path = entry.map_err(io(&dir))?.path();
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

/// The digests that point to the note with `file_id` at wire `path`: by
/// `metadata.note_fileId`, or, only for a digest without one, by `sourcePath` (relative to the
/// device storage root, like `Note/x.note`, so the wire path without its first segment). The
/// id is `metadata.unique_identifier` (the `digest_id` of the note's text box), else the
/// server id.
///
/// A copy keyed by its path (`path:<md5>`) has the `FILE_ID` of its original, so it gets the
/// original's digests too.
pub fn digests_of(digests: &[Digest], file_id: Option<&str>, path: &str) -> Vec<DigestRef> {
    let device_path = path.split_once('/').map_or(path, |(_, rest)| rest);
    digests
        .iter()
        .filter(|d| {
            let note_file_id = d
                .metadata
                .as_deref()
                .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
                .and_then(|m| m.get("note_fileId")?.as_str().map(str::to_string))
                .filter(|id| !id.is_empty());
            match note_file_id {
                Some(id) => file_id == Some(id.as_str()),
                None => d
                    .source_path
                    .as_deref()
                    .is_some_and(|p| p.trim_start_matches('/') == device_path),
            }
        })
        .map(|d| DigestRef {
            id: d.identity().unwrap_or_else(|| d.id.to_string()),
            time_ms: d.creation_time.and_then(|t| u64::try_from(t).ok()),
            source: None,
        })
        .collect()
}

/// The file of the note at wire `path`: `accounts/<slug>/files/<path>`.
pub fn note_file(data: &Path, slug: &str, path: &str) -> Result<PathBuf> {
    let wire = WirePath::parse(path)?;
    Ok(wire.to_fs(&account_dir(data, slug).join("files")))
}

/// Whether the note file at wire `path` is missing: it is not there, and the account's
/// `files/` folder is. Any other error (a folder in its place, no access) is no answer, and
/// without `files/` nothing is known, so neither counts as missing.
pub fn note_missing(data: &Path, slug: &str, path: &str) -> bool {
    let Ok(file) = note_file(data, slug, path) else {
        return false;
    };
    matches!(std::fs::metadata(&file), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
        && account_dir(data, slug).join("files").is_dir()
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
    fn digests_are_found_by_file_id_or_source_path() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(digests(tmp.path(), "jordi").unwrap().is_empty());
        let dir = tmp.path().join("accounts/jordi/digests/items");
        std::fs::create_dir_all(&dir).unwrap();
        let write = |id: u64, meta: &str, source: &str, deleted: &str| {
            let d = Digest {
                id,
                metadata: Some(meta.to_string()),
                source_path: Some(source.to_string()),
                source_type: Some(2),
                is_deleted: deleted.to_string(),
                creation_time: Some(5),
                ..Digest::default()
            };
            std::fs::write(
                dir.join(format!("{id}.json")),
                serde_json::to_vec(&d).unwrap(),
            )
            .unwrap();
        };
        write(
            1,
            r#"{"note_fileId":"F1","unique_identifier":"u1"}"#,
            "Note/old.note",
            "N",
        );
        write(2, r#"{"unique_identifier":"u2"}"#, "Note/a.note", "N");
        write(
            3,
            r#"{"note_fileId":"F1","unique_identifier":"u3"}"#,
            "Note/a.note",
            "Y",
        );
        write(
            4,
            r#"{"note_fileId":"F9","unique_identifier":"u4"}"#,
            "Note/b.note",
            "N",
        );
        // Another note's digest at this note's path: matched by its FILE_ID only.
        write(
            6,
            r#"{"note_fileId":"F9","unique_identifier":"u6"}"#,
            "Note/a.note",
            "N",
        );
        // No metadata at all: by path, with the server id.
        write(7, "", "Note/a.note", "N");
        std::fs::write(dir.join("5.json"), "{").unwrap();
        let all = digests(tmp.path(), "jordi").unwrap();
        assert_eq!(
            all.iter().map(|d| d.id).collect::<Vec<_>>(),
            [1, 2, 4, 6, 7]
        );
        let found = digests_of(&all, Some("F1"), "NOTE/Note/a.note");
        let ids: Vec<&str> = found.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["u1", "u2", "7"]);
        assert_eq!(found[0].time_ms, Some(5));
        assert!(digests_of(&all, None, "NOTE/Note/c.note").is_empty());
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
