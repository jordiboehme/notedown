//! `notedown cloud import-app-data`: fill an account's digests from the device's `.snbak`
//! backup, so the server holds them before the device's first digest sync (a digest the server
//! lacks would otherwise be deleted on the device, `docs/sync-protocol.md` 5.7).
//!
//! - The backup is a ZIP with a `backup/` root. `backup/DIGEST/knowledge.json` (required) holds
//!   the digests, `knowledge_bases.json` (optional) the groups and `handwrite/<name>.mark` the
//!   handwritten comments. Everything else (`Plugin/`, `digest.db`) is ignored.
//! - A digest keeps the cloud id the device knows (`serviceId`). One that has none (never
//!   synced) gets the next free id and is reported in [`AppImportReport::new_ids`].
//! - The account must hold the device files first (the `imported` marker, protect on or off),
//!   and no digests or to-dos, unless `replace` clears them (soft delete for digests and
//!   groups, removal for to-dos, all journaled).
//! - Refused before anything is written: a `.mark` that a digest names but the zip lacks, two
//!   digests (or groups) with the same id, a zip without `knowledge.json`.
//! - Writes happen under the store lock, one journal batch per record (origin `cli`). The
//!   `app_data_imported` marker goes in last, so a failed import leaves the gate closed.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

use serde::Deserialize;
use serde_json::{Map, Value};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::{AppData, Digest, DigestGroup, valid_mark_name};
use crate::auth::{Accounts, AppDataImported};
use crate::journal::{Journal, Origin};
use crate::store::Store;
use crate::{Error, Result, io_err};

const KNOWLEDGE: &str = "backup/DIGEST/knowledge.json";
const KNOWLEDGE_BASES: &str = "backup/DIGEST/knowledge_bases.json";
const HANDWRITE_DIR: &str = "backup/DIGEST/handwrite/";
/// Largest `.mark` accepted (the upload cap of the server).
const MAX_MARK_BYTES: u64 = 32 * 1024 * 1024;
/// Largest `knowledge.json` accepted.
const MAX_JSON_BYTES: u64 = 256 * 1024 * 1024;

/// Options of [`import_app_data`].
#[derive(Debug, Clone, Default)]
pub struct AppImportOptions {
    /// Read and check everything, write nothing.
    pub dry_run: bool,
    /// Clear the account's digests and to-dos first, instead of refusing.
    pub replace: bool,
}

/// What [`import_app_data`] did (or, for a dry run, would do).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppImportReport {
    pub digests: u64,
    pub groups: u64,
    /// Handwritten comments (`.mark` files) copied.
    pub marks: u64,
    /// Records the device did not know by a cloud id, with the id they got.
    pub new_ids: Vec<String>,
    /// Entries of the backup that were not imported (`.mark` files no digest names).
    pub skipped: Vec<String>,
    /// One line per digest: id, source path, first 40 characters of the content.
    pub mapping: Vec<String>,
    /// Records of the account removed by `replace`.
    pub replaced: u64,
}

fn failed(message: impl Into<String>) -> Error {
    Error::Invalid {
        what: "app data import",
        message: message.into(),
    }
}

/// A digest of `knowledge.json`; only the fields the mapping reads.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Knowledge {
    service_id: Value,
    content: Option<String>,
    comment_str: Option<String>,
    comment_handwrite_name: Option<String>,
    #[serde(rename = "handwriteMD5")]
    handwrite_md5: Option<String>,
    #[serde(rename = "dataMD5")]
    data_md5: Option<String>,
    creation_time: Option<i64>,
    last_modified_time: Option<i64>,
    source_path: Option<String>,
    source_type: Option<i64>,
    knowledge_base_unique_attribute: Option<String>,
    metadata: Value,
}

/// The backup, read and checked.
struct Backup {
    digests: Vec<Digest>,
    groups: Vec<DigestGroup>,
    /// Name -> bytes, for the marks the digests name.
    marks: BTreeMap<String, Vec<u8>>,
    skipped: Vec<String>,
}

fn read_entry(
    zip: &mut zip::ZipArchive<std::fs::File>,
    name: &str,
    limit: u64,
) -> Result<Option<Vec<u8>>> {
    let mut entry = match zip.by_name(name) {
        Ok(e) => e,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(failed(format!("{name}: {e}"))),
    };
    if entry.size() > limit {
        return Err(failed(format!("{name} is larger than {limit} bytes")));
    }
    let mut out = Vec::with_capacity(entry.size() as usize);
    entry
        .read_to_end(&mut out)
        .map_err(|e| failed(format!("{name}: {e}")))?;
    Ok(Some(out))
}

/// A cloud id: a positive JSON integer, or a decimal string of one. `None` for 0, null and
/// anything else.
fn service_id(v: &Value) -> Option<u64> {
    let id = match v {
        Value::Number(n) => n.as_u64()?,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    (id > 0 && id <= i64::MAX as u64).then_some(id)
}

/// The metadata as the JSON text the record keeps (the device stores a string; an object is
/// written out compactly).
fn metadata_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

fn read_backup(path: &Path) -> Result<Backup> {
    let file = std::fs::File::open(path).map_err(io_err(path))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| failed(format!("{} is not a readable .snbak: {e}", path.display())))?;
    let knowledge = read_entry(&mut zip, KNOWLEDGE, MAX_JSON_BYTES)?.ok_or_else(|| {
        failed(format!(
            "{} has no {KNOWLEDGE}; it is no digest backup",
            path.display()
        ))
    })?;
    let entries: Vec<Knowledge> = serde_json::from_slice(&knowledge)
        .map_err(|e| failed(format!("{KNOWLEDGE} cannot be read: {e}")))?;
    let mut bases: Vec<Map<String, Value>> =
        match read_entry(&mut zip, KNOWLEDGE_BASES, MAX_JSON_BYTES)? {
            Some(bytes) if !bytes.iter().all(u8::is_ascii_whitespace) => {
                serde_json::from_slice(&bytes)
                    .map_err(|e| failed(format!("{KNOWLEDGE_BASES} cannot be read: {e}")))?
            }
            _ => Vec::new(),
        };

    let mut digests = Vec::new();
    let mut groups = Vec::new();
    let mut seen: BTreeSet<u64> = BTreeSet::new();
    let mut dup: BTreeSet<u64> = BTreeSet::new();

    for k in entries {
        let id = service_id(&k.service_id);
        if let Some(id) = id
            && !seen.insert(id)
        {
            dup.insert(id);
        }
        let metadata = metadata_text(&k.metadata);
        let author = metadata
            .as_deref()
            .and_then(|m| serde_json::from_str::<Map<String, Value>>(m).ok())
            .and_then(|m| match m.get("author") {
                Some(Value::String(a)) => Some(a.clone()),
                _ => None,
            });
        digests.push(Digest {
            id: id.unwrap_or(0),
            unique_identifier: Some(String::new()),
            parent_unique_identifier: Some(k.knowledge_base_unique_attribute.unwrap_or_default()),
            content: k.content,
            source_path: k.source_path,
            source_type: k.source_type,
            md5_hash: k.data_md5,
            metadata,
            comment_str: k.comment_str,
            handwrite_inner_name: k.comment_handwrite_name.clone(),
            comment_handwrite_name: k.comment_handwrite_name,
            handwrite_md5: k.handwrite_md5,
            creation_time: k.creation_time,
            last_modified_time: k.last_modified_time,
            author,
            ..Digest::default()
        });
    }

    for (i, base) in bases.iter_mut().enumerate() {
        let id = base.remove("serviceId").as_ref().and_then(service_id);
        // The device's local row id means nothing to the server.
        base.remove("id");
        let mut g: DigestGroup = serde_json::from_value(Value::Object(std::mem::take(base)))
            .map_err(|e| failed(format!("{KNOWLEDGE_BASES}, entry {}: {e}", i + 1)))?;
        if let Some(id) = id {
            if !seen.insert(id) {
                dup.insert(id);
            }
            g.id = id;
        }
        groups.push(g);
    }
    if !dup.is_empty() {
        return Err(failed(format!(
            "ids used more than once in the backup: {}",
            dup.iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    // Handwritten comments the digests name.
    let mut marks = BTreeMap::new();
    let mut missing = BTreeSet::new();
    for d in &digests {
        let name = d.comment_handwrite_name.as_deref().unwrap_or("");
        if name.is_empty() || marks.contains_key(name) || missing.contains(name) {
            continue;
        }
        if !valid_mark_name(name) {
            return Err(failed(format!(
                "digest {:?} names the handwritten comment {name:?}, which is no valid .mark name",
                d.source_path.as_deref().unwrap_or("")
            )));
        }
        match read_entry(&mut zip, &format!("{HANDWRITE_DIR}{name}"), MAX_MARK_BYTES)? {
            Some(bytes) => {
                marks.insert(name.to_string(), bytes);
            }
            None => {
                missing.insert(name.to_string());
            }
        }
    }
    if !missing.is_empty() {
        return Err(failed(format!(
            "the backup lacks {} named by digests:\n  {}",
            if missing.len() == 1 {
                "this handwritten comment".to_string()
            } else {
                format!("these {} handwritten comments", missing.len())
            },
            missing
                .iter()
                .map(|n| format!("{HANDWRITE_DIR}{n}"))
                .collect::<Vec<_>>()
                .join("\n  ")
        )));
    }
    let mut skipped = Vec::new();
    for name in zip.file_names().map(String::from).collect::<Vec<_>>() {
        if let Some(rest) = name.strip_prefix(HANDWRITE_DIR)
            && !rest.is_empty()
            && !rest.ends_with('/')
            && !marks.contains_key(rest)
        {
            skipped.push(name);
        }
    }
    skipped.sort();

    Ok(Backup {
        digests,
        groups,
        marks,
        skipped,
    })
}

/// First 40 characters of the content, on one line.
fn excerpt(content: Option<&str>) -> String {
    content
        .unwrap_or("")
        .chars()
        .take(40)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// The refusal for an account that holds records; `counts` are digests, digest groups, to-dos,
/// to-do groups and to-do sorts.
fn present_refusal(slug: &str, c: [usize; 5]) -> Error {
    failed(format!(
        "account {slug} already holds {} digests, {} digest groups, {} to-dos, {} to-do groups \
         and {} to-do sorts; use --replace to clear them first",
        c[0], c[1], c[2], c[3], c[4]
    ))
}

/// Give the records without a cloud id new ids above `next` and above the backup's ids, set
/// the owner, and describe the digests. Returns the `new_ids` and `mapping` lines.
fn assign_ids(backup: &mut Backup, mut next: u64, user_id: u64) -> (Vec<String>, Vec<String>) {
    for id in backup
        .digests
        .iter()
        .map(|d| d.id)
        .chain(backup.groups.iter().map(|g| g.id))
    {
        next = next.max(id.saturating_add(1));
    }
    let mut new_ids = Vec::new();
    let mut mapping = Vec::new();
    for d in &mut backup.digests {
        if d.id == 0 {
            d.id = next;
            next += 1;
            new_ids.push(format!(
                "{}: {}",
                d.source_path.as_deref().unwrap_or(""),
                d.id
            ));
        }
        d.user_id = user_id;
        mapping.push(format!(
            "{}  {}  {}",
            d.id,
            d.source_path.as_deref().unwrap_or(""),
            excerpt(d.content.as_deref())
        ));
    }
    for g in &mut backup.groups {
        if g.id == 0 {
            g.id = next;
            next += 1;
            new_ids.push(format!(
                "group {}: {}",
                g.name.as_deref().unwrap_or(""),
                g.id
            ));
        }
    }
    (new_ids, mapping)
}

/// Live records and the largest file-name id in `dir` (`<id>.json` files), read without the
/// store lock. A file that cannot be read or parsed is an error, as in the strict lists. A
/// missing folder is empty.
fn scan_records(dir: &Path) -> Result<(usize, u64)> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(e) => return Err(io_err(dir)(e)),
    };
    let (mut live, mut max_id) = (0, 0);
    for e in entries {
        let path = e.map_err(io_err(dir))?.path();
        let Some(stem) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".json"))
            .filter(|n| !n.starts_with('.'))
        else {
            continue;
        };
        let bytes = std::fs::read(&path).map_err(io_err(&path))?;
        let record: Map<String, Value> = serde_json::from_slice(&bytes)
            .map_err(|e| failed(format!("{}: {e}", path.display())))?;
        if record.get("isDeleted").and_then(Value::as_str) != Some("Y") {
            live += 1;
        }
        if let Ok(id) = stem.parse::<u64>() {
            max_id = max_id.max(id);
        }
    }
    Ok((live, max_id))
}

/// The read-only check of [`import_app_data`] for `--dry-run`: the same refusals and counts,
/// and nothing is written. It takes no lock and needs no journal, so a pending commit stays
/// as it is and no folder is created. `replace` says whether `--replace` was given.
pub fn dry_run_app_data(
    data: &Path,
    slug: &str,
    snbak: Option<&Path>,
    replace: bool,
) -> Result<AppImportReport> {
    let account = Accounts::new(data)
        .get(slug)?
        .ok_or_else(|| Error::NoAccount(slug.to_string()))?;
    if account.imported.is_none() {
        return Err(failed(
            "import the device files first (notedown cloud import)",
        ));
    }
    let mut backup = match snbak {
        Some(path) => read_backup(path)?,
        None => Backup {
            digests: Vec::new(),
            groups: Vec::new(),
            marks: BTreeMap::new(),
            skipped: Vec::new(),
        },
    };
    let dir = data.join("accounts").join(slug);
    let (digests, max_item) = scan_records(&dir.join(super::DIGESTS_DIR).join("items"))?;
    let (groups, max_group) = scan_records(&dir.join(super::DIGESTS_DIR).join("groups"))?;
    let (tasks, _) = scan_records(&dir.join(super::TASKS_DIR).join("items"))?;
    let (task_groups, _) = scan_records(&dir.join(super::TASKS_DIR).join("groups"))?;
    let sorts = std::fs::read_dir(dir.join(super::TASKS_DIR).join("sort"))
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
                .count()
        })
        .unwrap_or(0);
    let present = digests + groups + tasks + task_groups + sorts;
    if present > 0 && !replace {
        return Err(present_refusal(
            slug,
            [digests, groups, tasks, task_groups, sorts],
        ));
    }
    let next = max_item
        .max(max_group)
        .saturating_add(1)
        .max(super::FIRST_DIGEST_ID);
    let (new_ids, mapping) = assign_ids(&mut backup, next, account.user_id);
    Ok(AppImportReport {
        digests: backup.digests.len() as u64,
        groups: backup.groups.len() as u64,
        marks: backup.marks.len() as u64,
        new_ids,
        skipped: backup.skipped,
        mapping,
        replaced: present as u64,
    })
}

/// Fill the account `slug` from the `.snbak` at `snbak`, or, with `None`, import nothing and
/// only set the marker (an account whose device has no digests). See the module docs.
pub fn import_app_data(
    data: &Path,
    slug: &str,
    snbak: Option<&Path>,
    options: &AppImportOptions,
    journal: &Journal,
) -> Result<AppImportReport> {
    if options.dry_run {
        return dry_run_app_data(data, slug, snbak, options.replace);
    }
    let accounts = Accounts::new(data);
    let account = accounts
        .get(slug)?
        .ok_or_else(|| Error::NoAccount(slug.to_string()))?;
    if account.imported.is_none() {
        return Err(failed(
            "import the device files first (notedown cloud import)",
        ));
    }
    let mut backup = match snbak {
        Some(path) => read_backup(path)?,
        None => Backup {
            digests: Vec::new(),
            groups: Vec::new(),
            marks: BTreeMap::new(),
            skipped: Vec::new(),
        },
    };

    let store = Store::open(data, slug)?;
    let guard = store.lock_recovered(journal)?;
    let app = AppData::new(&store);
    let live_digests: Vec<Digest> = app
        .digests_strict(&guard)?
        .into_iter()
        .filter(Digest::is_live)
        .collect();
    let live_groups: Vec<DigestGroup> = app
        .digest_groups_strict(&guard)?
        .into_iter()
        .filter(DigestGroup::is_live)
        .collect();
    let live_tasks: Vec<_> = app
        .tasks_strict(&guard)?
        .into_iter()
        .filter(|t| t.is_live())
        .collect();
    let task_groups: Vec<_> = app
        .task_groups_strict(&guard)?
        .into_iter()
        .filter(|g| g.is_live())
        .collect();
    let sorts = app.task_sorts_strict(&guard)?;
    let present =
        live_digests.len() + live_groups.len() + live_tasks.len() + task_groups.len() + sorts.len();
    if present > 0 && !options.replace {
        return Err(present_refusal(
            slug,
            [
                live_digests.len(),
                live_groups.len(),
                live_tasks.len(),
                task_groups.len(),
                sorts.len(),
            ],
        ));
    }

    let next = app.next_digest_id(&guard)?;
    let (new_ids, mapping) = assign_ids(&mut backup, next, account.user_id);
    let report = AppImportReport {
        digests: backup.digests.len() as u64,
        groups: backup.groups.len() as u64,
        marks: backup.marks.len() as u64,
        new_ids,
        skipped: backup.skipped,
        mapping,
        replaced: present as u64,
    };

    // Close the gate before the first write: a failed import must not leave it open.
    set_marker(&accounts, slug, None)?;
    if options.replace {
        for d in &live_digests {
            app.delete_digest(&guard, d.id, Origin::Cli, None, journal)?;
        }
        for g in &live_groups {
            app.delete_digest_group(&guard, g.id, Origin::Cli, None, journal)?;
        }
        for t in &live_tasks {
            app.delete_task(&guard, &t.task_id, Origin::Cli, None, journal)?;
        }
        for g in &task_groups {
            app.delete_task_group(&guard, &g.task_list_id, Origin::Cli, None, journal)?;
        }
        for s in &sorts {
            app.delete_task_sort(&guard, &s.task_list_id)?;
        }
    }
    for (name, bytes) in &backup.marks {
        app.put_mark(&guard, name, bytes)?;
    }
    for g in &backup.groups {
        app.put_digest_group(&guard, g, Origin::Cli, None, journal)?;
    }
    for d in &backup.digests {
        app.put_digest(&guard, d, Origin::Cli, None, journal)?;
    }
    let marker = AppDataImported {
        at: OffsetDateTime::from_unix_timestamp_nanos(journal.now_ms() as i128 * 1_000_000)
            .unwrap_or(OffsetDateTime::UNIX_EPOCH)
            .format(&Rfc3339)
            .expect("rfc3339 formatting"),
        source: snbak.map(|p| p.display().to_string()).unwrap_or_default(),
        digests: report.digests,
        groups: report.groups,
        marks: report.marks,
    };
    set_marker(&accounts, slug, Some(marker))?;
    drop(guard);
    Ok(report)
}

/// Write or clear the `app_data_imported` marker under the account lock (the store lock is
/// held).
fn set_marker(accounts: &Accounts, slug: &str, marker: Option<AppDataImported>) -> Result<()> {
    let _lock = accounts.lock(slug)?;
    let mut account = accounts
        .get(slug)?
        .ok_or_else(|| Error::NoAccount(slug.to_string()))?;
    if account.app_data_imported == marker {
        return Ok(());
    }
    account.app_data_imported = marker;
    accounts.save(&account)
}
