//! The account state of a diagnose bundle (spec 2.2), the real values the final check looks
//! for, and the dry-run scan (spec 2.3). Everything but the scan is read straight from disk,
//! without a lock, so the server is never held up.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use super::check::Needles;
use super::filter::{Class, Scrub, class_of};
use crate::auth::{Accounts, Devices};
use crate::journal::Journal;
use crate::store::{
    FILES_DIR, LockTry, PENDING_FILE, RECYCLE_DIR, SCAN_HOLD_FILE, ScanHold, ScanOptions,
    ScanReport, Store,
};
use crate::{Result, io_err};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceState {
    pub serial: String,
    pub firmware: Option<String>,
    pub bound: bool,
    pub last_login_ms: u64,
}

/// One account, as found on disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountState {
    pub slug: String,
    pub files: u64,
    pub folders: u64,
    pub notes: u64,
    pub bytes: u64,
    pub recycle_entries: u64,
    pub recycle_bytes: u64,
    pub digests: u64,
    pub digest_groups: u64,
    pub marks: u64,
    pub todo_groups: u64,
    pub todos: u64,
    pub devices: Vec<DeviceState>,
    pub hold: Option<ScanHold>,
    /// The hold marker is there but cannot be read.
    pub hold_unreadable: bool,
    pub pending: bool,
    /// The last journal `seq` of the account (filled from the journal).
    pub last_seq: u64,
    /// Events of the account after the `render` cursor (filled from the journal).
    pub render_behind: Option<u64>,
}

/// The state of account `slug`, read from disk without a lock.
pub fn account_state(data: &Path, slug: &str) -> Result<AccountState> {
    let dir = Accounts::new(data).account_dir(slug);
    let mut s = AccountState {
        slug: slug.to_string(),
        ..AccountState::default()
    };
    walk(&dir.join(FILES_DIR), &mut |path, meta| {
        if meta.is_dir() {
            s.folders += 1;
        } else {
            s.files += 1;
            s.bytes += meta.len();
            if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("note"))
            {
                s.notes += 1;
            }
        }
    })?;
    walk(&dir.join(RECYCLE_DIR), &mut |path, meta| {
        if meta.is_file() && path.file_name().is_some_and(|n| n != "index.jsonl") {
            s.recycle_entries += 1;
            s.recycle_bytes += meta.len();
        }
    })?;
    s.digests = count_files(&dir.join("digests/items"), "json")?;
    s.digest_groups = count_files(&dir.join("digests/groups"), "json")?;
    s.marks = count_files(&dir.join("digests/marks"), "mark")?;
    s.todos = count_files(&dir.join("tasks/items"), "json")?;
    s.todo_groups = count_files(&dir.join("tasks/groups"), "json")?;
    s.devices = Devices::new(data)
        .list(slug)?
        .into_iter()
        .map(|d| DeviceState {
            serial: d.serial,
            firmware: d.firmware,
            bound: d.bound,
            last_login_ms: d.last_login_ms,
        })
        .collect();
    match std::fs::read(dir.join(SCAN_HOLD_FILE)) {
        Ok(bytes) => match serde_json::from_slice::<ScanHold>(&bytes) {
            Ok(hold) => s.hold = Some(hold),
            Err(_) => s.hold_unreadable = true,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err(&dir.join(SCAN_HOLD_FILE))(e)),
    }
    s.pending = dir.join(PENDING_FILE).exists();
    Ok(s)
}

/// Every entry below `root` (not `root` itself), without following symlinks; hidden names
/// (a leading dot) are left out, as the store does. A missing `root` is empty.
fn walk(root: &Path, each: &mut dyn FnMut(&Path, &std::fs::Metadata)) -> Result<()> {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_err(root)(e)),
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        each(&path, &meta);
        if meta.is_dir() {
            walk(&path, each)?;
        }
    }
    Ok(())
}

fn count_files(dir: &Path, ext: &str) -> Result<u64> {
    let mut n = 0;
    walk(dir, &mut |path, meta| {
        if meta.is_file() && path.extension().is_some_and(|e| e == ext) {
            n += 1;
        }
    })?;
    Ok(n)
}

/// Collect the real values of the given accounts: the needles of the final check and the
/// values the free-text scrub replaces.
pub fn collect_needles(
    data: &Path,
    slugs: &[String],
    needles: &mut Needles,
    scrub: &mut Scrub,
) -> Result<()> {
    scrub.data_dirs.push(data.to_string_lossy().into_owned());
    if let Ok(real) = data.canonicalize() {
        scrub.data_dirs.push(real.to_string_lossy().into_owned());
    }
    if let Some(home) = dirs::home_dir() {
        if let Some(user) = home.file_name() {
            needles.add(&user.to_string_lossy());
        }
        scrub.home = Some(home.to_string_lossy().into_owned());
    }
    let accounts = Accounts::new(data);
    for slug in slugs {
        needles.add(slug);
        scrub.slugs.push(slug.clone());
        if let Some(a) = accounts.get(slug)? {
            for person in [&a.email, &a.user_name] {
                needles.add(person);
                if let Some((local, domain)) = person.split_once('@') {
                    needles.add(local);
                    needles.add(domain);
                }
                scrub.people.push(person.clone());
            }
        }
        for d in Devices::new(data).list(slug)? {
            needles.add(&d.serial);
            scrub.serials.push(d.serial.clone());
            if let Some(name) = d.name.filter(|n| !n.is_empty()) {
                needles.add(&name);
                scrub.people.push(name);
            }
        }
        let dir = accounts.account_dir(slug);
        let files = dir.join(FILES_DIR);
        walk(&files, &mut |path, _| {
            if let Some(name) = path.file_name() {
                let name = name.to_string_lossy();
                needles.add(&name);
                scrub.names.push(name.into_owned());
            }
        })?;
        let index = dir.join(RECYCLE_DIR).join("index.jsonl");
        if let Ok(text) = std::fs::read_to_string(&index) {
            for line in text.lines() {
                if let Ok(v) = serde_json::from_str::<Value>(line)
                    && let Some(p) = v.get("path").and_then(Value::as_str)
                {
                    needles.add_path(p);
                    scrub.names.extend(p.split('/').map(String::from));
                }
            }
        }
        for sub in [
            "digests/items",
            "digests/groups",
            "tasks/items",
            "tasks/groups",
        ] {
            walk(&dir.join(sub), &mut |path, meta| {
                if meta.is_file()
                    && let Ok(bytes) = std::fs::read(path)
                    && let Ok(v) = serde_json::from_slice::<Value>(&bytes)
                {
                    add_texts(needles, "", &v);
                }
            })?;
        }
    }
    scrub.names.retain(|n| !n.is_empty());
    scrub.names.sort();
    scrub.names.dedup();
    Ok(())
}

/// Every string of a digest or to-do record that the filter would replace or hide.
fn add_texts(needles: &mut Needles, key: &str, v: &Value) {
    match v {
        Value::String(s) => match class_of(key) {
            Class::Keep | Class::Vocab => {}
            Class::Name => {
                needles.add(s);
                needles.add_path(s);
            }
            _ => needles.add(s),
        },
        Value::Array(items) => items.iter().for_each(|i| add_texts(needles, key, i)),
        Value::Object(map) => map.iter().for_each(|(k, i)| add_texts(needles, k, i)),
        _ => {}
    }
}

/// The dry-run scan of one account, or why it was skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanResult {
    Done(ScanReport),
    Skipped(String),
}

/// The plan of `notedown cloud scan --dry-run` for one account. The store lock is tried, then
/// tried again until `wait` is over; a busy store or an unfinished change skips the scan.
pub fn dry_run_scan(data: &Path, slug: &str, min_files: u64, wait: Duration) -> ScanResult {
    let run = || -> Result<ScanResult> {
        let store = Store::open_unlocked(data, slug)?;
        let guard = match store.lock_unless_pending_within(wait)? {
            LockTry::Locked(g) => g,
            LockTry::Busy => {
                return Ok(ScanResult::Skipped(format!(
                    "the store was busy for {} s",
                    wait.as_secs_f32()
                )));
            }
            LockTry::Pending => {
                return Ok(ScanResult::Skipped(
                    "an unfinished change is pending".to_string(),
                ));
            }
        };
        let journal = Journal::open_no_create(data);
        let mut opts = ScanOptions::all(min_files);
        opts.dry_run = true;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        Ok(ScanResult::Done(
            store.scan(&guard, &journal, &opts, now_ms)?,
        ))
    };
    run().unwrap_or_else(|e| ScanResult::Skipped(format!("the scan failed: {e}")))
}
