//! Digest records: items, groups, tags and the handwritten comments (`docs/sync-protocol.md`
//! 5.1, 5.5).

use std::fs::File;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{
    AppData, Change, FIRST_DIGEST_ID, FIRST_TAG_ID, Mode, NO, YES, change, read_record, record_ids,
    write_record,
};
use crate::journal::{DigestGroupInfo, DigestInfo, EventBody, Journal, Origin};
use crate::store::WriteGuard;
use crate::{Error, Result, io_err};

const ITEMS_DIR: &str = "items";
const GROUPS_DIR: &str = "groups";
const MARKS_DIR: &str = "marks";
const TAGS_FILE: &str = "tags.json";

/// One digest: the wire record of 5.1 in its order, the server-only times, then the fields the
/// device sent that are not named here. Strings the device did not send are `null`, as in
/// Ratta's answers. Every field has a default, so a request body reads as a `Digest`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Digest {
    /// The server id (Ratta snowflake or [`AppData::next_digest_id`]); an exact integer.
    #[serde(deserialize_with = "super::null_default")]
    pub id: u64,
    /// Unused, always null on the device; kept as sent.
    pub file_id: Value,
    pub name: Option<String>,
    /// The owner's user id; set by the caller.
    #[serde(deserialize_with = "super::null_default")]
    pub user_id: u64,
    /// Empty on items from the device; the identity is `metadata.unique_identifier`.
    pub unique_identifier: Option<String>,
    /// The group of the item; empty = no group.
    pub parent_unique_identifier: Option<String>,
    pub content: Option<String>,
    pub source_path: Option<String>,
    pub data_source: Value,
    /// `1` document, `2` note.
    pub source_type: Option<i64>,
    /// `"N"` for items.
    #[serde(deserialize_with = "super::null_as_no")]
    pub is_summary_group: String,
    pub description: Option<String>,
    pub tags: Option<String>,
    pub md5_hash: Option<String>,
    /// A JSON object as a string (5.2), kept byte-exact so numbers stay numbers.
    pub metadata: Option<String>,
    pub comment_str: Option<String>,
    pub comment_handwrite_name: Option<String>,
    pub handwrite_inner_name: Option<String>,
    #[serde(rename = "handwriteMD5")]
    pub handwrite_md5: Option<String>,
    /// Device time, ms.
    pub creation_time: Option<i64>,
    /// Device time, ms.
    pub last_modified_time: Option<i64>,
    /// `"Y"` once soft-deleted.
    #[serde(deserialize_with = "super::null_as_no")]
    pub is_deleted: String,
    /// Server time of the first write, ms; set by [`AppData::put_digest`].
    #[serde(deserialize_with = "super::null_default")]
    pub create_time: u64,
    /// Server time of the last write, ms; set by [`AppData::put_digest`].
    #[serde(deserialize_with = "super::null_default")]
    pub update_time: u64,
    pub author: Option<String>,
    /// Server only: when the server first stored the record (ms).
    #[serde(rename = "created_at_ms", skip_serializing_if = "Option::is_none")]
    pub created_at_ms: Option<u64>,
    /// Server only: when the server last wrote the record (ms).
    #[serde(rename = "updated_at_ms", skip_serializing_if = "Option::is_none")]
    pub updated_at_ms: Option<u64>,
    /// Fields of the device that are not named above, sent back unchanged.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Digest {
    fn default() -> Self {
        Digest {
            id: 0,
            file_id: Value::Null,
            name: None,
            user_id: 0,
            unique_identifier: None,
            parent_unique_identifier: None,
            content: None,
            source_path: None,
            data_source: Value::Null,
            source_type: None,
            is_summary_group: NO.into(),
            description: None,
            tags: None,
            md5_hash: None,
            metadata: None,
            comment_str: None,
            comment_handwrite_name: None,
            handwrite_inner_name: None,
            handwrite_md5: None,
            creation_time: None,
            last_modified_time: None,
            is_deleted: NO.into(),
            create_time: 0,
            update_time: 0,
            author: None,
            created_at_ms: None,
            updated_at_ms: None,
            extra: Map::new(),
        }
    }
}

impl Digest {
    /// False once soft-deleted.
    pub fn is_live(&self) -> bool {
        self.is_deleted != YES
    }

    /// The digest's identity: `metadata.unique_identifier` when the metadata is a JSON object
    /// with a non-empty string there (5.2).
    pub fn identity(&self) -> Option<String> {
        let meta: Map<String, Value> = serde_json::from_str(self.metadata.as_deref()?).ok()?;
        match meta.get("unique_identifier") {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            _ => None,
        }
    }

    /// The record as the device gets it: without the server-only fields.
    pub fn wire(&self) -> Digest {
        Digest {
            created_at_ms: None,
            updated_at_ms: None,
            ..self.clone()
        }
    }

    pub(crate) fn info(&self) -> DigestInfo {
        DigestInfo {
            id: self.id,
            unique_identifier: self.identity().unwrap_or_default(),
            source_path: self.source_path.clone().unwrap_or_default(),
            source_type: self.source_type,
            md5_hash: self.md5_hash.clone().unwrap_or_default(),
        }
    }
}

/// One digest group ("category"): its fields of the 5.1 record in that order, then the fields
/// the device sent that are not named here. Records written before the group had `createTime`
/// and `updateTime` fields kept them among the extra fields; they read into the fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DigestGroup {
    /// The server id, in the id space of the digests.
    #[serde(deserialize_with = "super::null_default")]
    pub id: u64,
    /// 32 hex, made by the device; items name it in `parentUniqueIdentifier`.
    pub unique_identifier: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub metadata: Option<String>,
    pub comment_str: Option<String>,
    pub comment_handwrite_name: Option<String>,
    pub handwrite_inner_name: Option<String>,
    pub md5_hash: Option<String>,
    /// Device time, ms.
    pub creation_time: Option<i64>,
    /// Device time, ms.
    pub last_modified_time: Option<i64>,
    /// `"Y"` once soft-deleted.
    #[serde(deserialize_with = "super::null_as_no")]
    pub is_deleted: String,
    /// Server time of the first write, ms (0: not known); kept by
    /// [`AppData::put_digest_group`].
    #[serde(deserialize_with = "super::null_default")]
    pub create_time: u64,
    /// Server time of the last write, ms; set by [`AppData::put_digest_group`].
    #[serde(deserialize_with = "super::null_default")]
    pub update_time: u64,
    /// Fields of the device that are not named above, sent back unchanged.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for DigestGroup {
    fn default() -> Self {
        DigestGroup {
            id: 0,
            unique_identifier: None,
            name: None,
            description: None,
            metadata: None,
            comment_str: None,
            comment_handwrite_name: None,
            handwrite_inner_name: None,
            md5_hash: None,
            creation_time: None,
            last_modified_time: None,
            is_deleted: NO.into(),
            create_time: 0,
            update_time: 0,
            extra: Map::new(),
        }
    }
}

impl DigestGroup {
    /// False once soft-deleted.
    pub fn is_live(&self) -> bool {
        self.is_deleted != YES
    }

    fn info(&self) -> DigestGroupInfo {
        DigestGroupInfo {
            id: self.id,
            name: self.name.clone().unwrap_or_default(),
        }
    }
}

/// One digest tag (5.3). All tags of an account are one file, `digests/tags.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DigestTag {
    pub id: u64,
    pub name: String,
    pub unique_identifier: String,
    /// Server time, ms.
    pub created_at: u64,
}

/// Ids start at [`FIRST_DIGEST_ID`]; 0 is what a request without an id reads as.
fn refuse_id_0(id: u64, what: &'static str) -> Result<()> {
    if id == 0 {
        return Err(Error::Invalid {
            what,
            message: "0 is no server id".into(),
        });
    }
    Ok(())
}

fn digest_event(c: Change, info: DigestInfo) -> EventBody {
    match c {
        Change::Created => EventBody::DigestCreated(info),
        Change::Updated => EventBody::DigestUpdated(info),
        Change::Deleted => EventBody::DigestDeleted(info),
    }
}

fn group_event(c: Change, info: DigestGroupInfo) -> EventBody {
    match c {
        Change::Created => EventBody::DigestGroupCreated(info),
        Change::Updated => EventBody::DigestGroupUpdated(info),
        Change::Deleted => EventBody::DigestGroupDeleted(info),
    }
}

impl AppData<'_> {
    fn item_path(&self, id: u64) -> PathBuf {
        self.digests_dir()
            .join(ITEMS_DIR)
            .join(format!("{id}.json"))
    }

    fn group_path(&self, id: u64) -> PathBuf {
        self.digests_dir()
            .join(GROUPS_DIR)
            .join(format!("{id}.json"))
    }

    fn digests_with(&self, guard: &WriteGuard, mode: Mode) -> Result<Vec<Digest>> {
        self.check(guard)?;
        let dir = self.digests_dir().join(ITEMS_DIR);
        let mut out: Vec<Digest> = self.load_all(&dir, mode)?;
        out.sort_by_key(|d| d.id);
        Ok(out)
    }

    /// Every digest, live and deleted, by id. A record file that cannot be read is skipped with
    /// a warning.
    pub fn digests(&self, guard: &WriteGuard) -> Result<Vec<Digest>> {
        self.digests_with(guard, Mode::Lenient)
    }

    /// [`AppData::digests`], but a record file that cannot be read is an error.
    pub fn digests_strict(&self, guard: &WriteGuard) -> Result<Vec<Digest>> {
        self.digests_with(guard, Mode::Strict)
    }

    /// The digest with `id`, live or deleted; `None` also for a record file that cannot be
    /// read (with a warning).
    pub fn digest(&self, guard: &WriteGuard, id: u64) -> Result<Option<Digest>> {
        self.check(guard)?;
        self.load(&self.item_path(id), Mode::Lenient)
    }

    /// Store `digest` under its id (a full replace; an old record file that cannot be read
    /// counts as absent), then journal `digest.created` (new, or
    /// back from a delete), `digest.updated` or `digest.deleted` (`isDeleted` turned `"Y"`).
    /// Sets `createTime` and `created_at_ms` (kept from an existing record), `updateTime` and
    /// `updated_at_ms` to the journal clock's now. Id 0 is refused (`Error::Invalid`).
    pub fn put_digest(
        &self,
        guard: &WriteGuard,
        digest: &Digest,
        origin: Origin,
        device: Option<&str>,
        journal: &Journal,
    ) -> Result<()> {
        refuse_id_0(digest.id, "digest id")?;
        let old = self.digest(guard, digest.id)?;
        let now = journal.now_ms();
        let created = old
            .as_ref()
            .and_then(|o| o.created_at_ms.or(Some(o.create_time)))
            .filter(|&t| t > 0)
            .unwrap_or(now);
        let mut record = digest.clone();
        record.create_time = created;
        record.update_time = now;
        record.created_at_ms = Some(created);
        record.updated_at_ms = Some(now);
        write_record(&self.item_path(record.id), &record)?;
        let was_live = old.as_ref().is_some_and(Digest::is_live);
        let bodies = change(was_live, record.is_live())
            .map(|c| digest_event(c, record.info()))
            .into_iter()
            .collect();
        self.journal(bodies, origin, device, journal)
    }

    /// Soft-delete the digest `id` (`isDeleted: "Y"`, the file stays) and journal
    /// `digest.deleted`. False when there is no live digest with that id.
    pub fn delete_digest(
        &self,
        guard: &WriteGuard,
        id: u64,
        origin: Origin,
        device: Option<&str>,
        journal: &Journal,
    ) -> Result<bool> {
        let Some(mut d) = self.digest(guard, id)?.filter(Digest::is_live) else {
            return Ok(false);
        };
        d.is_deleted = YES.into();
        self.put_digest(guard, &d, origin, device, journal)?;
        Ok(true)
    }

    /// The id for a new digest or group: the largest id of all digests and groups (deleted ones
    /// too, ids are never reused) plus one, or [`FIRST_DIGEST_ID`].
    pub fn next_digest_id(&self, guard: &WriteGuard) -> Result<u64> {
        self.check(guard)?;
        let items = record_ids(&self.digests_dir().join(ITEMS_DIR))?;
        let groups = record_ids(&self.digests_dir().join(GROUPS_DIR))?;
        let max = items.last().into_iter().chain(groups.last()).max();
        match max {
            None => Ok(FIRST_DIGEST_ID),
            Some(m) => m.checked_add(1).ok_or_else(|| Error::Invalid {
                what: "digest id",
                message: "the id space is exhausted".into(),
            }),
        }
    }

    fn digest_groups_with(&self, guard: &WriteGuard, mode: Mode) -> Result<Vec<DigestGroup>> {
        self.check(guard)?;
        let mut out: Vec<DigestGroup> =
            self.load_all(&self.digests_dir().join(GROUPS_DIR), mode)?;
        out.sort_by_key(|g| g.id);
        Ok(out)
    }

    /// Every digest group, live and deleted, by id. A record file that cannot be read is
    /// skipped with a warning.
    pub fn digest_groups(&self, guard: &WriteGuard) -> Result<Vec<DigestGroup>> {
        self.digest_groups_with(guard, Mode::Lenient)
    }

    /// [`AppData::digest_groups`], but a record file that cannot be read is an error.
    pub fn digest_groups_strict(&self, guard: &WriteGuard) -> Result<Vec<DigestGroup>> {
        self.digest_groups_with(guard, Mode::Strict)
    }

    /// The digest group with `id`, live or deleted; `None` also for a record file that cannot
    /// be read (with a warning).
    pub fn digest_group(&self, guard: &WriteGuard, id: u64) -> Result<Option<DigestGroup>> {
        self.check(guard)?;
        self.load(&self.group_path(id), Mode::Lenient)
    }

    /// Store `group` under its id (a full replace), then journal `digest_group.created`,
    /// `.updated` or `.deleted` as [`AppData::put_digest`] does. `createTime` is kept from the
    /// stored record, else taken from `group` (an import), else now; `updateTime` is now. Id 0
    /// is refused.
    pub fn put_digest_group(
        &self,
        guard: &WriteGuard,
        group: &DigestGroup,
        origin: Origin,
        device: Option<&str>,
        journal: &Journal,
    ) -> Result<()> {
        refuse_id_0(group.id, "digest group id")?;
        let old = self.digest_group(guard, group.id)?;
        let now = journal.now_ms();
        let mut record = group.clone();
        record.create_time = [old.as_ref().map_or(0, |o| o.create_time), group.create_time]
            .into_iter()
            .find(|&t| t > 0)
            .unwrap_or(now);
        record.update_time = now;
        write_record(&self.group_path(record.id), &record)?;
        let was_live = old.as_ref().is_some_and(DigestGroup::is_live);
        let bodies = change(was_live, record.is_live())
            .map(|c| group_event(c, record.info()))
            .into_iter()
            .collect();
        self.journal(bodies, origin, device, journal)
    }

    /// Soft-delete the group `id` and journal `digest_group.deleted`. Its items are not
    /// touched here. False when there is no live group with that id.
    pub fn delete_digest_group(
        &self,
        guard: &WriteGuard,
        id: u64,
        origin: Origin,
        device: Option<&str>,
        journal: &Journal,
    ) -> Result<bool> {
        let Some(mut g) = self.digest_group(guard, id)?.filter(DigestGroup::is_live) else {
            return Ok(false);
        };
        g.is_deleted = YES.into();
        self.put_digest_group(guard, &g, origin, device, journal)?;
        Ok(true)
    }

    /// All tags (`digests/tags.json`), empty when the file does not exist.
    pub fn tags(&self, guard: &WriteGuard) -> Result<Vec<DigestTag>> {
        self.check(guard)?;
        Ok(read_record(&self.digests_dir().join(TAGS_FILE))?.unwrap_or_default())
    }

    /// Replace all tags. Tags are not journaled.
    pub fn put_tags(&self, guard: &WriteGuard, tags: &[DigestTag]) -> Result<()> {
        self.check(guard)?;
        write_record(&self.digests_dir().join(TAGS_FILE), &tags)
    }

    /// The id for a new tag: the largest tag id plus one, or [`FIRST_TAG_ID`].
    pub fn next_tag_id(&self, guard: &WriteGuard) -> Result<u64> {
        Ok(self
            .tags(guard)?
            .iter()
            .map(|t| t.id.saturating_add(1))
            .max()
            .unwrap_or(FIRST_TAG_ID))
    }

    /// Where the handwritten comment `name` lives (`digests/marks/<name>`); an error for a
    /// name that [`super::valid_mark_name`] refuses. No existence check.
    pub fn mark_path(&self, guard: &WriteGuard, name: &str) -> Result<PathBuf> {
        self.check(guard)?;
        if !super::valid_mark_name(name) {
            return Err(Error::Invalid {
                what: "mark name",
                message: format!("{name:?} is no plain <name>.mark of at most 64 bytes"),
            });
        }
        Ok(self.digests_dir().join(MARKS_DIR).join(name))
    }

    /// The bytes of the handwritten comment `name`, `None` when it is missing.
    pub fn mark(&self, guard: &WriteGuard, name: &str) -> Result<Option<Vec<u8>>> {
        let path = self.mark_path(guard, name)?;
        match std::fs::read(&path) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err(&path)(e)),
        }
    }

    /// Store the handwritten comment `name` atomically (an existing one is replaced). Not
    /// journaled: the digest's `update/summary` that names it is.
    pub fn put_mark(&self, guard: &WriteGuard, name: &str, bytes: &[u8]) -> Result<()> {
        let path = self.mark_path(guard, name)?;
        crate::fsutil::write_atomic(&path, bytes, false)
    }

    /// The folder of the handwritten comments (`digests/marks`). An upload streams into a temp
    /// file here without the store lock and [`AppData::put_mark_file`] renames it then.
    pub fn marks_dir(&self) -> PathBuf {
        self.digests_dir().join(MARKS_DIR)
    }

    /// Make the closed temp file `tmp` (in [`AppData::marks_dir`]) the handwritten comment
    /// `name`: a rename in the same folder, so the comment is replaced atomically. Not
    /// journaled, as [`AppData::put_mark`].
    pub fn put_mark_file(&self, guard: &WriteGuard, name: &str, tmp: &Path) -> Result<()> {
        let path = self.mark_path(guard, name)?;
        if tmp.parent() != path.parent() {
            return Err(Error::Invalid {
                what: "mark upload",
                message: format!("{} is not in {}", tmp.display(), self.marks_dir().display()),
            });
        }
        crate::fsutil::rename_durable(tmp, &path)
    }

    /// The handwritten comment `name` opened for reading, with its size; `None` when it is
    /// missing.
    pub fn open_mark(&self, guard: &WriteGuard, name: &str) -> Result<Option<(File, u64)>> {
        let path = self.mark_path(guard, name)?;
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(&path)(e)),
        };
        let meta = file.metadata().map_err(io_err(&path))?;
        Ok(meta.is_file().then_some((file, meta.len())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appdata::testutil::{Fixture, T0};

    fn full_digest() -> Digest {
        let mut extra = Map::new();
        extra.insert("zNew".into(), serde_json::json!({"b": 1, "a": [1, 2]}));
        extra.insert("aNew".into(), Value::from("x"));
        Digest {
            id: 895_334_188_016_730_112,
            file_id: Value::Null,
            name: None,
            user_id: 1_000_001,
            unique_identifier: Some(String::new()),
            parent_unique_identifier: Some("g1".into()),
            content: Some("text".into()),
            source_path: Some("Document/y.pdf".into()),
            data_source: Value::Null,
            source_type: Some(1),
            is_summary_group: "N".into(),
            description: None,
            tags: Some(String::new()),
            md5_hash: Some("m".into()),
            metadata: Some(
                r#"{"document_location_data":"[{\"chapter\":2,\"endPosition\":995,\"page\":0,\"startPosition\":940}]","source_size":3344196,"unique_identifier":"f6444dce52bc2460c932d42aba0f7133","author":"Jordi"}"#.into(),
            ),
            comment_str: Some("c".into()),
            comment_handwrite_name: Some("h.mark".into()),
            handwrite_inner_name: Some("h.mark".into()),
            handwrite_md5: Some("hm".into()),
            creation_time: Some(1_700_000_000_000),
            last_modified_time: Some(1_700_000_000_001),
            is_deleted: "N".into(),
            create_time: 5,
            update_time: 6,
            author: Some("Jordi".into()),
            created_at_ms: Some(5),
            updated_at_ms: Some(6),
            extra,
        }
    }

    #[test]
    fn digest_serializes_in_wire_order_with_extra_last() {
        let text = serde_json::to_string(&full_digest()).unwrap();
        let expected = concat!(
            r#"{"id":895334188016730112,"fileId":null,"name":null,"userId":1000001,"#,
            r#""uniqueIdentifier":"","parentUniqueIdentifier":"g1","content":"text","#,
            r#""sourcePath":"Document/y.pdf","dataSource":null,"sourceType":1,"#,
            r#""isSummaryGroup":"N","description":null,"tags":"","md5Hash":"m","metadata":"#,
            r#""{\"document_location_data\":\"[{\\\"chapter\\\":2,\\\"endPosition\\\":995,\\\"page\\\":0,\\\"startPosition\\\":940}]\",\"source_size\":3344196,\"unique_identifier\":\"f6444dce52bc2460c932d42aba0f7133\",\"author\":\"Jordi\"}","#,
            r#""commentStr":"c","commentHandwriteName":"h.mark","handwriteInnerName":"h.mark","#,
            r#""handwriteMD5":"hm","creationTime":1700000000000,"lastModifiedTime":1700000000001,"#,
            r#""isDeleted":"N","createTime":5,"updateTime":6,"author":"Jordi","#,
            r#""created_at_ms":5,"updated_at_ms":6,"aNew":"x","zNew":{"a":[1,2],"b":1}}"#
        );
        assert_eq!(text, expected);
        let back: Digest = serde_json::from_str(&text).unwrap();
        assert_eq!(back, full_digest());
    }

    #[test]
    fn wire_drops_the_server_fields_only() {
        let text = serde_json::to_string(&full_digest().wire()).unwrap();
        assert!(!text.contains("_at_ms"));
        assert!(text.contains(r#""author":"Jordi","aNew":"x""#));
    }

    #[test]
    fn a_request_body_reads_with_defaults_and_keeps_unknown_fields() {
        let d: Digest =
            serde_json::from_str(r#"{"content":"x","sourceType":2,"newField":[1]}"#).unwrap();
        assert_eq!(d.id, 0);
        assert_eq!(d.is_deleted, "N");
        assert_eq!(d.is_summary_group, "N");
        assert_eq!(d.extra.get("newField"), Some(&serde_json::json!([1])));
    }

    #[test]
    fn source_size_in_metadata_stays_an_integer() {
        let f = Fixture::new();
        let app = AppData::new(&f.store);
        let g = f.store.lock_recovered(&f.journal).unwrap();
        app.put_digest(&g, &full_digest(), Origin::Cli, None, &f.journal)
            .unwrap();
        let back = app.digest(&g, 895_334_188_016_730_112).unwrap().unwrap();
        assert_eq!(back.metadata, full_digest().metadata);
        let meta: Value = serde_json::from_str(back.metadata.as_deref().unwrap()).unwrap();
        assert!(meta["source_size"].is_u64());
        assert_eq!(meta["source_size"].as_u64(), Some(3_344_196));
        assert_eq!(
            back.identity().as_deref(),
            Some("f6444dce52bc2460c932d42aba0f7133")
        );
    }

    #[test]
    fn a_snowflake_id_round_trips_exactly() {
        let f = Fixture::new();
        let app = AppData::new(&f.store);
        let g = f.store.lock_recovered(&f.journal).unwrap();
        app.put_digest(&g, &full_digest(), Origin::Cli, None, &f.journal)
            .unwrap();
        let path = f
            .store
            .account_dir()
            .join("digests/items/895334188016730112.json");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("{\n  \"id\": 895334188016730112,\n"),
            "{text}"
        );
        assert!(text.ends_with("}\n"));
        let all = app.digests(&g).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, 895_334_188_016_730_112);
        // The store sets the server times from the journal clock.
        assert_eq!(all[0].create_time, T0 as u64);
        assert_eq!(all[0].updated_at_ms, Some(T0 as u64));
    }

    #[test]
    fn an_update_keeps_the_create_time() {
        let f = Fixture::new();
        let app = AppData::new(&f.store);
        let g = f.store.lock_recovered(&f.journal).unwrap();
        let d = Digest {
            id: 3,
            ..Digest::default()
        };
        app.put_digest(&g, &d, Origin::Cli, None, &f.journal)
            .unwrap();
        f.set_now(T0 + 1000);
        app.put_digest(&g, &d, Origin::Cli, None, &f.journal)
            .unwrap();
        let back = app.digest(&g, 3).unwrap().unwrap();
        assert_eq!(back.create_time, T0 as u64);
        assert_eq!(back.created_at_ms, Some(T0 as u64));
        assert_eq!(back.update_time, T0 as u64 + 1000);
    }

    #[test]
    fn next_digest_id_follows_imported_snowflakes_of_items_and_groups() {
        let f = Fixture::new();
        let app = AppData::new(&f.store);
        let g = f.store.lock_recovered(&f.journal).unwrap();
        assert_eq!(app.next_digest_id(&g).unwrap(), FIRST_DIGEST_ID);
        app.put_digest(&g, &full_digest(), Origin::Cli, None, &f.journal)
            .unwrap();
        assert_eq!(app.next_digest_id(&g).unwrap(), 895_334_188_016_730_113);
        let group = DigestGroup {
            id: 895_334_188_016_730_200,
            ..DigestGroup::default()
        };
        app.put_digest_group(&g, &group, Origin::Cli, None, &f.journal)
            .unwrap();
        assert_eq!(app.next_digest_id(&g).unwrap(), 895_334_188_016_730_201);
        // A deleted record keeps its id: ids are never reused.
        app.delete_digest_group(&g, 895_334_188_016_730_200, Origin::Cli, None, &f.journal)
            .unwrap();
        assert_eq!(app.next_digest_id(&g).unwrap(), 895_334_188_016_730_201);
    }

    #[test]
    fn soft_delete_keeps_the_file() {
        let f = Fixture::new();
        let app = AppData::new(&f.store);
        let g = f.store.lock_recovered(&f.journal).unwrap();
        app.put_digest(&g, &full_digest(), Origin::Cli, None, &f.journal)
            .unwrap();
        let id = full_digest().id;
        assert!(
            app.delete_digest(&g, id, Origin::Cli, None, &f.journal)
                .unwrap()
        );
        assert!(
            !app.delete_digest(&g, id, Origin::Cli, None, &f.journal)
                .unwrap()
        );
        let back = app.digest(&g, id).unwrap().unwrap();
        assert_eq!(back.is_deleted, "Y");
        assert_eq!(back.content.as_deref(), Some("text"));
        let text = std::fs::read_to_string(
            f.store
                .account_dir()
                .join(format!("digests/items/{id}.json")),
        )
        .unwrap();
        assert!(text.contains("\"isDeleted\": \"Y\""));
        // The group delete is soft too.
        let group = DigestGroup {
            id: 9,
            ..DigestGroup::default()
        };
        app.put_digest_group(&g, &group, Origin::Cli, None, &f.journal)
            .unwrap();
        assert!(
            app.delete_digest_group(&g, 9, Origin::Cli, None, &f.journal)
                .unwrap()
        );
        assert_eq!(app.digest_group(&g, 9).unwrap().unwrap().is_deleted, "Y");
        assert_eq!(app.digest_groups(&g).unwrap().len(), 1);
    }

    #[test]
    fn group_serializes_in_wire_order() {
        let mut extra = Map::new();
        extra.insert("userId".into(), Value::from(1_000_001));
        let group = DigestGroup {
            id: 1,
            unique_identifier: Some("u".into()),
            name: Some("n".into()),
            description: Some("d".into()),
            metadata: Some("{}".into()),
            comment_str: Some("c".into()),
            comment_handwrite_name: Some("h".into()),
            handwrite_inner_name: Some("i".into()),
            md5_hash: Some("m".into()),
            creation_time: Some(2),
            last_modified_time: Some(3),
            is_deleted: "N".into(),
            create_time: 4,
            update_time: 5,
            extra,
        };
        let text = serde_json::to_string(&group).unwrap();
        assert_eq!(
            text,
            concat!(
                r#"{"id":1,"uniqueIdentifier":"u","name":"n","description":"d","metadata":"{}","#,
                r#""commentStr":"c","commentHandwriteName":"h","handwriteInnerName":"i","#,
                r#""md5Hash":"m","creationTime":2,"lastModifiedTime":3,"isDeleted":"N","#,
                r#""createTime":4,"updateTime":5,"userId":1000001}"#
            )
        );
        assert_eq!(serde_json::from_str::<DigestGroup>(&text).unwrap(), group);
    }

    #[test]
    fn an_older_group_record_with_its_times_after_the_extra_fields_reads() {
        // Records written before the group had time fields kept them among the extra fields.
        let text =
            r#"{"id":1,"name":"n","isDeleted":"N","userId":3,"createTime":7,"updateTime":8}"#;
        let g: DigestGroup = serde_json::from_str(text).unwrap();
        assert_eq!((g.create_time, g.update_time), (7, 8));
        assert!(!g.extra.contains_key("createTime"));
        assert_eq!(g.extra.get("userId"), Some(&Value::from(3)));
    }

    #[test]
    fn a_group_write_keeps_the_create_time_and_stamps_the_update_time() {
        let f = Fixture::new();
        let app = AppData::new(&f.store);
        let g = f.store.lock_recovered(&f.journal).unwrap();
        // A new group without a createTime gets now.
        let fresh = DigestGroup {
            id: 3,
            ..DigestGroup::default()
        };
        app.put_digest_group(&g, &fresh, Origin::Cli, None, &f.journal)
            .unwrap();
        let back = app.digest_group(&g, 3).unwrap().unwrap();
        assert_eq!((back.create_time, back.update_time), (T0 as u64, T0 as u64));
        // A new group with a createTime (an import) keeps it.
        let imported = DigestGroup {
            id: 4,
            create_time: 1_700_000_000_000,
            update_time: 1_700_000_000_500,
            ..DigestGroup::default()
        };
        app.put_digest_group(&g, &imported, Origin::Cli, None, &f.journal)
            .unwrap();
        let back = app.digest_group(&g, 4).unwrap().unwrap();
        assert_eq!(back.create_time, 1_700_000_000_000);
        assert_eq!(back.update_time, T0 as u64);
        // A later write keeps the stored createTime, whatever the new record says.
        f.set_now(T0 + 1000);
        let edit = DigestGroup {
            id: 4,
            name: Some("x".into()),
            create_time: 0,
            ..DigestGroup::default()
        };
        app.put_digest_group(&g, &edit, Origin::Device, None, &f.journal)
            .unwrap();
        app.delete_digest_group(&g, 3, Origin::Device, None, &f.journal)
            .unwrap();
        let back = app.digest_group(&g, 4).unwrap().unwrap();
        assert_eq!(back.create_time, 1_700_000_000_000);
        assert_eq!(back.update_time, T0 as u64 + 1000);
        let deleted = app.digest_group(&g, 3).unwrap().unwrap();
        assert_eq!(deleted.create_time, T0 as u64);
        assert_eq!(deleted.update_time, T0 as u64 + 1000);
    }

    #[test]
    fn tags_are_one_file_with_their_own_ids() {
        let f = Fixture::new();
        let app = AppData::new(&f.store);
        let g = f.store.lock_recovered(&f.journal).unwrap();
        assert!(app.tags(&g).unwrap().is_empty());
        assert_eq!(app.next_tag_id(&g).unwrap(), FIRST_TAG_ID);
        let tag = DigestTag {
            id: 4,
            name: "red".into(),
            unique_identifier: "u".into(),
            created_at: 7,
        };
        app.put_tags(&g, std::slice::from_ref(&tag)).unwrap();
        assert_eq!(app.tags(&g).unwrap(), vec![tag]);
        assert_eq!(app.next_tag_id(&g).unwrap(), 5);
        let text =
            std::fs::read_to_string(f.store.account_dir().join("digests/tags.json")).unwrap();
        assert!(
            text.contains("\"uniqueIdentifier\": \"u\",\n    \"createdAt\": 7"),
            "{text}"
        );
        assert_eq!(app.next_digest_id(&g).unwrap(), FIRST_DIGEST_ID);
    }

    #[test]
    fn marks_are_bytes_under_a_checked_name() {
        let f = Fixture::new();
        let app = AppData::new(&f.store);
        let g = f.store.lock_recovered(&f.journal).unwrap();
        let name = "0123456789abcdef0123456789abcdef.mark";
        assert_eq!(app.mark(&g, name).unwrap(), None);
        app.put_mark(&g, name, b"one").unwrap();
        app.put_mark(&g, name, b"two").unwrap();
        assert_eq!(app.mark(&g, name).unwrap().as_deref(), Some(&b"two"[..]));
        assert!(
            f.store
                .account_dir()
                .join("digests/marks")
                .join(name)
                .is_file()
        );
        assert!(app.put_mark(&g, "../x.mark", b"x").is_err());
        assert!(app.mark(&g, ".hidden.mark").is_err());
    }
}
