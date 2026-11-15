//! The page records of an account and what the pipeline makes of them: the digests of a note
//! and the page-index key of a link. The records themselves are read by
//! [`notedown_cloud::read`], through [`crate::source::ServerData`].

use std::collections::BTreeMap;

use notedown_cloud::appdata::Digest;
use notedown_note::DigestRef;

pub use notedown_cloud::read::{Record, Records};

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
    use notedown_cloud::pages::PageRecord;
    use notedown_cloud::read::digests;

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
}
