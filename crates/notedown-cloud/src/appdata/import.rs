//! `notedown cloud import-app-data`: fill an account's digests from the device's `.snbak`
//! backup, so the server holds them with the ids the device knows before the device's first
//! digest sync. Without them the device sees an empty cloud and has to send every digest back
//! (`docs/sync-protocol.md` 5.7).
//!
//! - The backup is a ZIP with a `backup/` root. `backup/DIGEST/knowledge.json` (required) holds
//!   the digests, `knowledge_bases.json` (optional) the groups and `handwrite/<name>.mark` the
//!   handwritten comments. Everything else (`Plugin/`, `digest.db`) is ignored.
//! - A digest or group keeps the cloud id the device knows (`serviceId`). One that has none
//!   (it never reached a cloud) is not imported and is reported in
//!   [`AppImportReport::left_to_device`]: the device adds it itself at its first digest sync.
//!   A server id the device never saw would come back to the device as a second copy.
//! - A digest whose group (`knowledgeBaseUniqueAttribute`) is no imported group's
//!   `uniqueIdentifier` is imported and reported in [`AppImportReport::without_group`].
//! - To-dos are never touched: the backup holds none, and a cleared to-do would be deleted on
//!   the device too.
//! - The account must hold the device files first (the `imported` marker, protect on or off),
//!   and no digests or digest groups, unless `replace` soft-deletes them first (journaled).
//! - Refused before anything is written: a `.mark` that an imported digest names but the zip
//!   lacks, two digests (or groups) with the same id, a zip without `knowledge.json`.
//! - Writes happen under the store lock, one journal batch per record (origin `cli`). The
//!   `app_data_imported` marker goes in last, so a failed import leaves the gate closed.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

use serde::Deserialize;
use serde_json::{Map, Value};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::{AppData, Digest, DigestGroup, MAX_MARK_BYTES, valid_mark_name};
use crate::auth::{Accounts, AppDataImported};
use crate::journal::{Journal, Origin};
use crate::store::Store;
use crate::{Error, Result, io_err};

const KNOWLEDGE: &str = "backup/DIGEST/knowledge.json";
const KNOWLEDGE_BASES: &str = "backup/DIGEST/knowledge_bases.json";
const HANDWRITE_DIR: &str = "backup/DIGEST/handwrite/";
/// Largest `knowledge.json` accepted.
const MAX_JSON_BYTES: u64 = 256 * 1024 * 1024;

/// Options of [`import_app_data`].
#[derive(Debug, Clone, Default)]
pub struct AppImportOptions {
    /// Read and check everything, write nothing.
    pub dry_run: bool,
    /// Soft-delete the account's digests and digest groups first, instead of refusing.
    pub replace: bool,
}

/// What [`import_app_data`] did (or, for a dry run, would do).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppImportReport {
    pub digests: u64,
    pub groups: u64,
    /// Handwritten comments (`.mark` files) copied.
    pub marks: u64,
    /// Digests and groups without a cloud id, not imported (the device adds them itself): one
    /// line each, `<source path>  <first 40 characters>` or `group <name>`.
    pub left_to_device: Vec<String>,
    /// Imported digests whose group is no imported group: one line each,
    /// `<id>  <source path>  group <unique identifier>`.
    pub without_group: Vec<String>,
    /// Entries of the backup that were not imported (`.mark` files no imported digest names).
    pub skipped: Vec<String>,
    /// One line per digest: id, source path, first 40 characters of the content.
    #[serde(skip)]
    pub mapping: Vec<String>,
    /// Digests and digest groups of the account soft-deleted by `replace`.
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
#[derive(Default)]
struct Backup {
    /// The digests with a cloud id.
    digests: Vec<Digest>,
    /// The groups with a cloud id.
    groups: Vec<DigestGroup>,
    /// Name -> bytes, for the marks the imported digests name.
    marks: BTreeMap<String, Vec<u8>>,
    skipped: Vec<String>,
    left_to_device: Vec<String>,
    without_group: Vec<String>,
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

/// A cloud id: a positive JSON integer up to `i64::MAX`, or a decimal string of one. `None` for
/// 0, null and anything else.
fn service_id(v: &Value) -> Option<u64> {
    let id = match v {
        Value::Number(n) => n.as_u64()?,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    (id > 0 && id <= i64::MAX as u64).then_some(id)
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

/// A server time of a backup group: a positive JSON integer, else 0.
fn backup_time(v: Option<&Value>) -> u64 {
    v.and_then(Value::as_u64).unwrap_or(0)
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
    let mut left_to_device = Vec::new();
    let mut seen: BTreeSet<u64> = BTreeSet::new();
    let mut dup: BTreeSet<u64> = BTreeSet::new();

    for k in entries {
        let Some(id) = service_id(&k.service_id) else {
            left_to_device.push(format!(
                "{}  {}",
                k.source_path.as_deref().unwrap_or(""),
                excerpt(k.content.as_deref())
            ));
            continue;
        };
        if !seen.insert(id) {
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
            id,
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
        // The device's local row id means nothing to the server. The server fields are set
        // here or by the store, whatever type the backup gives them.
        base.remove("id");
        let create_time = backup_time(base.get("createTime"));
        for key in ["userId", "isSummaryGroup", "createTime", "updateTime"] {
            base.remove(key);
        }
        let mut g: DigestGroup = serde_json::from_value(Value::Object(std::mem::take(base)))
            .map_err(|e| failed(format!("{KNOWLEDGE_BASES}, entry {}: {e}", i + 1)))?;
        let Some(id) = id else {
            left_to_device.push(format!("group {}", g.name.as_deref().unwrap_or("")));
            continue;
        };
        if !seen.insert(id) {
            dup.insert(id);
        }
        g.id = id;
        // The server's createTime when the backup has one, else the device's creationTime.
        g.create_time = Some(create_time)
            .filter(|&t| t > 0)
            .unwrap_or_else(|| g.creation_time.unwrap_or(0).max(0) as u64);
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
    // A name zip cannot decode is left out of this list of skipped files.
    for name in zip
        .file_names()
        .filter_map(|n| n.ok().map(|n| n.into_owned()))
        .collect::<Vec<_>>()
    {
        if let Some(rest) = name.strip_prefix(HANDWRITE_DIR)
            && !rest.is_empty()
            && !rest.ends_with('/')
            && !marks.contains_key(rest)
        {
            skipped.push(name);
        }
    }
    skipped.sort();

    // Digests whose group is not imported (a group of the device that never reached a cloud,
    // or one the backup lacks).
    let identifiers: BTreeSet<&str> = groups
        .iter()
        .filter_map(|g| g.unique_identifier.as_deref())
        .collect();
    let without_group = digests
        .iter()
        .filter_map(|d| {
            let parent = d.parent_unique_identifier.as_deref().unwrap_or("");
            (!parent.is_empty() && !identifiers.contains(parent)).then(|| {
                format!(
                    "{}  {}  group {parent}",
                    d.id,
                    d.source_path.as_deref().unwrap_or("")
                )
            })
        })
        .collect();

    Ok(Backup {
        digests,
        groups,
        marks,
        skipped,
        left_to_device,
        without_group,
    })
}

/// The refusal for an account that holds digests or digest groups.
fn present_refusal(slug: &str, digests: usize, groups: usize) -> Error {
    failed(format!(
        "account {slug} already holds {digests} digests and {groups} digest groups; use \
         --replace to clear them first"
    ))
}

/// Set the owner of the digests and describe them: one `mapping` line each.
fn own_and_map(backup: &mut Backup, user_id: u64) -> Vec<String> {
    backup
        .digests
        .iter_mut()
        .map(|d| {
            d.user_id = user_id;
            format!(
                "{}  {}  {}",
                d.id,
                d.source_path.as_deref().unwrap_or(""),
                excerpt(d.content.as_deref())
            )
        })
        .collect()
}

/// The report of an import of `backup` that clears `replaced` records first.
fn report(backup: &mut Backup, user_id: u64, replaced: usize) -> AppImportReport {
    let mapping = own_and_map(backup, user_id);
    AppImportReport {
        digests: backup.digests.len() as u64,
        groups: backup.groups.len() as u64,
        marks: backup.marks.len() as u64,
        left_to_device: backup.left_to_device.clone(),
        without_group: backup.without_group.clone(),
        skipped: backup.skipped.clone(),
        mapping,
        replaced: replaced as u64,
    }
}

/// The live records in `dir` (`<id>.json` files), read without the store lock. A file that
/// cannot be read or parsed is an error, as in the strict lists. A missing folder is empty.
fn scan_records(dir: &Path) -> Result<usize> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(io_err(dir)(e)),
    };
    let mut live = 0;
    for e in entries {
        let path = e.map_err(io_err(dir))?.path();
        let record_file = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".json"))
            .is_some_and(|n| !n.starts_with('.'));
        if !record_file {
            continue;
        }
        let bytes = std::fs::read(&path).map_err(io_err(&path))?;
        let record: Map<String, Value> = serde_json::from_slice(&bytes)
            .map_err(|e| failed(format!("{}: {e}", path.display())))?;
        if record.get("isDeleted").and_then(Value::as_str) != Some("Y") {
            live += 1;
        }
    }
    Ok(live)
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
        None => Backup::default(),
    };
    let dir = data.join("accounts").join(slug).join(super::DIGESTS_DIR);
    let digests = scan_records(&dir.join("items"))?;
    let groups = scan_records(&dir.join("groups"))?;
    if digests + groups > 0 && !replace {
        return Err(present_refusal(slug, digests, groups));
    }
    Ok(report(&mut backup, account.user_id, digests + groups))
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
        None => Backup::default(),
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
    if (!live_digests.is_empty() || !live_groups.is_empty()) && !options.replace {
        return Err(present_refusal(slug, live_digests.len(), live_groups.len()));
    }
    let report = report(
        &mut backup,
        account.user_id,
        live_digests.len() + live_groups.len(),
    );

    // Close the gate before the first write: a failed import must not leave it open.
    set_marker(&accounts, slug, None)?;
    if options.replace {
        for d in &live_digests {
            app.delete_digest(&guard, d.id, Origin::Cli, None, journal)?;
        }
        for g in &live_groups {
            app.delete_digest_group(&guard, g.id, Origin::Cli, None, journal)?;
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
