//! `notedown cloud backup-device`: a fresh, byte-exact copy of the device over Browse & Access
//! (the device's web page, port 8089), as the input of `notedown cloud import`.
//!
//! - The device answers `GET /` and `GET <folder uri>` with an HTML page that holds one line
//!   `    const json = '<json>'`. The JSON is `{"deviceName", "fileList": [...]}`; each entry
//!   has `date` (`YYYY-MM-DD HH:MM`, device local time), `isDirectory`, `name`, `size` and
//!   `uri`. `uri` is already URL-encoded (`+` for a space, `%CC%88`) and is requested exactly
//!   as given: encoding it again gives empty files.
//! - The local name is the listing's `name`, byte-exact, never a decoded `uri`. A name with
//!   `/` or NUL, `.`, `..`, or one that differs from a sibling only in case or Unicode
//!   normalization (the local file system may ignore both) is refused and counted as an
//!   error. Nothing is written outside the target folder.
//! - Each file goes to `<name>.part` first (a short dot-name when that would be too long), its
//!   size is checked against the listing, the date becomes its modification time, then it is
//!   renamed. Three tries per file and per folder listing.
//! - The SD card is skipped unless asked for. The device lists it at the root as `REMOVABLE0`
//!   or under the card's name with a `uri` below `/REMOVABLE0/`; with `--include-sd` it is
//!   stored below `REMOVABLE0/` either way.
//! - `<dir>/backup.json` is written with `"complete": false` before the first download and
//!   again at the end with `"complete": true`, the counts per top folder, the total bytes, the
//!   errors and the dates that did not parse. A killed run leaves `false`, and the import
//!   refuses it.
//! - `resume` keeps a file only when its size and its date both match the listing. At the end
//!   it reports every local file the device no longer lists (deleted or renamed on the device
//!   since the earlier run) in the `stale` list of `backup.json` and on the progress lines. The
//!   import refuses such a backup, because it would give the device those files back: the
//!   final backup before a switch is a fresh run into an empty folder.
//! - Timeouts: 10 s to connect, and 60 s for the answer headers and for each read of the body
//!   (reqwest's blocking `timeout` applies per send and per read, not to the whole transfer).
//!   So a stalled download fails within a minute, and a large slow one that keeps moving does
//!   not.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

use crate::import::{BACKUP_FILE, SD_CARD};
use crate::{Error, Result, io_err};

/// Tries per file download and per folder listing.
pub const TRIES: u32 = 3;
/// The key of files directly at the device root in [`BackupReport::folders`].
pub const ROOT_KEY: &str = "/";
/// Longest local name (bytes) that still gets the `.part` suffix.
const MAX_PART_NAME: usize = 250;

/// Options of [`backup`].
#[derive(Debug, Clone, Copy)]
pub struct BackupOptions {
    /// Also copy `REMOVABLE0`, the SD card.
    pub include_sd: bool,
    /// Continue into a folder that is not empty; files whose size matches are kept.
    pub resume: bool,
    /// The device's offset from UTC, to read the listing dates.
    pub utc_offset: UtcOffset,
}

/// The SD card: the device lists it at the root, either as `REMOVABLE0` or under the card's own
/// name (Jordi's Nomad, firmware 3.29.42: `name` "Extended", `uri` "/REMOVABLE0/Extended").
fn is_sd_card(entry: &DeviceEntry) -> bool {
    entry.name == SD_CARD
        || entry.uri == format!("/{SD_CARD}")
        || entry.uri.starts_with(&format!("/{SD_CARD}/"))
}

/// One entry of a device listing. Unknown fields are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DeviceEntry {
    #[serde(default)]
    pub date: String,
    #[serde(rename = "isDirectory")]
    pub is_directory: bool,
    pub name: String,
    #[serde(default)]
    pub size: u64,
    pub uri: String,
}

#[derive(Deserialize)]
struct Listing {
    #[serde(rename = "fileList")]
    file_list: Vec<DeviceEntry>,
}

/// Files and bytes of one top folder.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FolderTotal {
    pub files: u64,
    pub bytes: u64,
}

/// An entry that could not be copied; `path` is relative to the device root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackupError {
    pub path: String,
    pub message: String,
}

/// The content of `backup.json`. Field order is the file order.
#[derive(Debug, Clone, Serialize)]
pub struct BackupReport {
    pub url: String,
    /// When the backup started (while `complete` is false) or finished, RFC 3339.
    pub time: String,
    /// False from the start of the run until it finished: a killed run leaves `false`, and
    /// `notedown cloud import` refuses such a backup.
    pub complete: bool,
    /// Per top folder (device name); files at the root count under [`ROOT_KEY`].
    pub folders: BTreeMap<String, FolderTotal>,
    /// Files in the backup (downloaded now or kept by `resume`).
    pub files: u64,
    pub bytes: u64,
    pub errors: Vec<BackupError>,
    /// Listing dates that did not parse (the file keeps the download time as its mtime).
    pub date_warnings: u64,
    /// One entry per date warning.
    pub warnings: Vec<BackupError>,
    /// `resume`: local files (paths from the device root) that the device no longer lists. A
    /// stale folder without files is listed with a trailing `/`. The import refuses a report
    /// with entries here.
    pub stale: Vec<String>,
    /// Files kept from an earlier run (`resume`).
    #[serde(skip)]
    pub skipped_existing: u64,
}

/// Read the listing from a device page: the line `const json = '<json>'`. Inside the quotes,
/// `\'` stands for `'`; every other backslash pair is a JSON escape and passes through. If that
/// does not give valid JSON (an unescaped `'` inside), the text up to the last `'` of the line
/// is tried. A page without the line, or with JSON that does not parse, is an error, never an
/// empty listing.
pub fn parse_page(html: &str) -> std::result::Result<Vec<DeviceEntry>, String> {
    const MARK: &str = "const json = '";
    let start = html
        .find(MARK)
        .ok_or_else(|| "the page has no `const json = '...'` line".to_string())?
        + MARK.len();
    let line = html[start..].split('\n').next().unwrap_or("");
    let (scanned, _) = unescape(line, true);
    let first = serde_json::from_str::<Listing>(&scanned);
    if let Ok(l) = first {
        return Ok(l.file_list);
    }
    let last = line.rfind('\'').unwrap_or(line.len());
    let (whole, _) = unescape(&line[..last], false);
    match serde_json::from_str::<Listing>(&whole) {
        Ok(l) => Ok(l.file_list),
        Err(e) => Err(format!("the listing JSON does not parse: {e}")),
    }
}

/// Undo the JS quote escape `\'`; keep other backslash pairs. With `stop`, end at the first
/// unescaped `'`. Returns the text and whether it stopped at a quote.
fn unescape(text: &str, stop: bool) -> (String, bool) {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('\'') => out.push('\''),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            },
            '\'' if stop => return (out, true),
            c => out.push(c),
        }
    }
    (out, false)
}

/// Why a listing name cannot be a local name.
fn bad_name(name: &str) -> Option<&'static str> {
    if name.is_empty() || name == "." || name == ".." {
        Some("the name is empty, . or ..")
    } else if name.contains('/') || name.contains('\0') {
        Some("the name contains / or NUL")
    } else {
        None
    }
}

/// The listing date `YYYY-MM-DD HH:MM` in `offset`, as a file time.
fn parse_date(date: &str, offset: UtcOffset) -> Option<SystemTime> {
    let b = date.as_bytes();
    if b.len() != 16 || b[4] != b'-' || b[7] != b'-' || b[10] != b' ' || b[13] != b':' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| date.get(r)?.parse::<u32>().ok();
    let day = time::Date::from_calendar_date(
        n(0..4)? as i32,
        time::Month::try_from(n(5..7)? as u8).ok()?,
        n(8..10)? as u8,
    )
    .ok()?;
    let at = day
        .with_hms(n(11..13)? as u8, n(14..16)? as u8, 0)
        .ok()?
        .assume_offset(offset);
    Some(at.into())
}

/// One thing that happened during [`backup`], for the caller to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupProgress<'a> {
    /// A file was downloaded or kept (`resume`). `files` and `bytes` are the running totals of
    /// the backup including this file; `path` is relative to the device root.
    Downloaded {
        files: u64,
        bytes: u64,
        path: &'a str,
    },
    /// An entry could not be copied (also in the report's `errors`).
    Error { path: &'a str, message: &'a str },
    /// A listing date did not parse (also in the report's `warnings`).
    Warning { path: &'a str, message: &'a str },
    /// The SD card was skipped.
    SkippedSd { name: &'a str },
    /// `resume`: a local file or folder (trailing `/`) the device no longer lists.
    Stale { path: &'a str },
}

struct Run<'a> {
    client: reqwest::blocking::Client,
    base: String,
    options: BackupOptions,
    report: BackupReport,
    progress: &'a mut dyn FnMut(BackupProgress<'_>),
    /// The backup folder, for paths in warnings.
    root: PathBuf,
}

/// Copy the device at `url` (for example `http://192.168.1.30:8089`) into `dir`, which must be
/// empty or missing unless `resume`. Errors of single entries are collected in the report (and
/// in `backup.json`); the call itself fails only when the backup cannot start or the report
/// cannot be written.
pub fn backup(
    url: &str,
    dir: &Path,
    options: &BackupOptions,
    progress: &mut dyn FnMut(BackupProgress<'_>),
) -> Result<BackupReport> {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => {
            if entries.next().is_some() && !options.resume {
                return Err(Error::Invalid {
                    what: "backup folder",
                    message: format!(
                        "{} is not empty; use --resume to continue an earlier backup",
                        dir.display()
                    ),
                });
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir).map_err(io_err(dir))?;
        }
        Err(e) => return Err(io_err(dir)(e)),
    }
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        // per request and per read: a stalled transfer fails and is tried again
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| Error::Invalid {
            what: "http client",
            message: e.to_string(),
        })?;
    let mut run = Run {
        client,
        base: url.trim_end_matches('/').to_string(),
        options: *options,
        report: BackupReport {
            url: url.to_string(),
            time: now_rfc3339(),
            complete: false,
            folders: BTreeMap::new(),
            files: 0,
            bytes: 0,
            errors: Vec::new(),
            date_warnings: 0,
            warnings: Vec::new(),
            stale: Vec::new(),
            skipped_existing: 0,
        },
        progress,
        root: dir.to_path_buf(),
    };
    // Written before the first download: a run that is killed leaves `complete: false`.
    let report_path = dir.join(BACKUP_FILE);
    crate::fsutil::write_atomic(&report_path, &crate::fsutil::json_bytes(&run.report), false)?;
    run.folder("/", dir, &[]);
    run.report.time = now_rfc3339();
    run.report.complete = true;
    let report = run.report;
    crate::fsutil::write_atomic(&report_path, &crate::fsutil::json_bytes(&report), false)?;
    Ok(report)
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("rfc3339 formatting")
}

impl Run<'_> {
    fn error(&mut self, path: String, message: String) {
        (self.progress)(BackupProgress::Error {
            path: &path,
            message: &message,
        });
        self.report.errors.push(BackupError { path, message });
    }

    fn url(&self, uri: &str) -> String {
        if uri.starts_with('/') {
            format!("{}{uri}", self.base)
        } else {
            format!("{}/{uri}", self.base)
        }
    }

    fn list(&self, uri: &str) -> std::result::Result<Vec<DeviceEntry>, String> {
        let mut last = String::new();
        for _ in 0..TRIES {
            let page = self
                .client
                .get(self.url(uri))
                .send()
                .map_err(|e| e.to_string())
                .and_then(|r| {
                    if r.status() != reqwest::StatusCode::OK {
                        return Err(format!("the device answered {}", r.status()));
                    }
                    r.bytes().map_err(|e| e.to_string())
                })
                .and_then(|b| {
                    String::from_utf8(b.to_vec()).map_err(|_| "the page is not UTF-8".to_string())
                })
                .and_then(|html| parse_page(&html));
            match page {
                Ok(list) => return Ok(list),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Copy the folder `uri` into `local`; `rel` is its path from the device root.
    fn folder(&mut self, uri: &str, local: &Path, rel: &[String]) {
        let shown = if rel.is_empty() {
            "/".to_string()
        } else {
            rel.join("/")
        };
        let list = match self.list(uri) {
            Ok(l) => l,
            Err(e) => {
                self.error(shown, format!("cannot list the folder: {e}"));
                return;
            }
        };
        let mut folded: HashMap<String, String> = HashMap::new();
        let siblings: HashSet<String> = list.iter().map(|e| e.name.clone()).collect();
        if self.options.resume {
            self.remove_stale_parts(local, &siblings, &shown);
            self.report_stale(local, &siblings, rel);
        }
        for entry in list {
            let mut here = rel.to_vec();
            here.push(entry.name.clone());
            let path = here.join("/");
            if let Some(why) = bad_name(&entry.name) {
                self.error(format!("{shown}: {:?}", entry.name), why.to_string());
                continue;
            }
            let sd_card = rel.is_empty() && is_sd_card(&entry);
            if sd_card && !self.options.include_sd {
                (self.progress)(BackupProgress::SkippedSd { name: &entry.name });
                continue;
            }
            if rel.is_empty() && entry.name == BACKUP_FILE {
                self.error(
                    path,
                    format!("the name is reserved for the report {BACKUP_FILE}"),
                );
                continue;
            }
            if let Some(other) = folded.insert(crate::store::fold(&entry.name), entry.name.clone())
            {
                self.error(
                    path,
                    format!("differs from {other:?} only in case or Unicode normalization"),
                );
                continue;
            }
            let mut target = local.join(&entry.name);
            if sd_card && entry.name != SD_CARD {
                // The device lists the card at the root under its own name (for example
                // "Extended") with a uri below /REMOVABLE0/. Keep it below REMOVABLE0 locally,
                // so the import skips it like any SD card content.
                target = local.join(SD_CARD).join(&entry.name);
                here = vec![SD_CARD.to_string(), entry.name.clone()];
            }
            if entry.is_directory {
                if let Err(e) = std::fs::create_dir_all(&target) {
                    self.error(path, format!("cannot create the folder: {e}"));
                    continue;
                }
                self.folder(&entry.uri, &target, &here);
            } else {
                let top = if rel.is_empty() {
                    ROOT_KEY.to_string()
                } else {
                    rel[0].clone()
                };
                match self.file(&entry, &target, &siblings) {
                    Ok(kept) => {
                        if kept {
                            self.report.skipped_existing += 1;
                        }
                        let t = self.report.folders.entry(top).or_default();
                        t.files += 1;
                        t.bytes += entry.size;
                        self.report.files += 1;
                        self.report.bytes += entry.size;
                        (self.progress)(BackupProgress::Downloaded {
                            files: self.report.files,
                            bytes: self.report.bytes,
                            path: &path,
                        });
                    }
                    Err(e) => self.error(path, e),
                }
            }
        }
    }

    /// `resume`: report every local entry of `local` that the device listing does not name: a
    /// file by its path, a folder by every file inside it (or by itself with a trailing `/`
    /// when it holds none). `backup.json` at the top and host junk (`.DS_Store`, `._*`) are no
    /// device files.
    fn report_stale(&mut self, local: &Path, siblings: &HashSet<String>, rel: &[String]) {
        let Ok(entries) = std::fs::read_dir(local) else {
            return;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !siblings.contains(n))
            .filter(|n| !(rel.is_empty() && n == BACKUP_FILE))
            .filter(|n| !crate::import::is_host_junk(n))
            .collect();
        names.sort();
        for name in names {
            let mut here = rel.to_vec();
            here.push(name.clone());
            self.stale_entry(&local.join(&name), &here);
        }
    }

    fn stale_entry(&mut self, path: &Path, rel: &[String]) {
        let shown = rel.join("/");
        let is_dir = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
        if !is_dir {
            (self.progress)(BackupProgress::Stale { path: &shown });
            self.report.stale.push(shown);
            return;
        }
        let before = self.report.stale.len();
        let mut names: Vec<String> = std::fs::read_dir(path)
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| !crate::import::is_host_junk(n))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        for name in names {
            let mut here = rel.to_vec();
            here.push(name.clone());
            self.stale_entry(&path.join(&name), &here);
        }
        if self.report.stale.len() == before {
            let folder = format!("{shown}/");
            (self.progress)(BackupProgress::Stale { path: &folder });
            self.report.stale.push(folder);
        }
    }

    /// `resume`: remove the unfinished downloads of an earlier run in `local`: every `*.part`
    /// file the listing does not name itself (`.nd-backup-*.part`, and `<name>.part` whether
    /// or not the device still lists `<name>`). Without this a leftover would stay next to the
    /// finished backup and refuse its import.
    fn remove_stale_parts(&mut self, local: &Path, siblings: &HashSet<String>, shown: &str) {
        let Ok(entries) = std::fs::read_dir(local) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let is_file = e.file_type().is_ok_and(|t| t.is_file());
            if !is_file || !name.ends_with(".part") || siblings.contains(&name) {
                continue;
            }
            if let Err(err) = std::fs::remove_file(e.path()) {
                let path = if shown == "/" {
                    name.clone()
                } else {
                    format!("{shown}/{name}")
                };
                self.error(
                    path,
                    format!("cannot remove the unfinished download: {err}"),
                );
            }
        }
    }

    /// Download one file. Returns true when `resume` kept an existing copy.
    fn file(
        &mut self,
        entry: &DeviceEntry,
        target: &Path,
        siblings: &HashSet<String>,
    ) -> std::result::Result<bool, String> {
        let mtime = parse_date(&entry.date, self.options.utc_offset);
        if mtime.is_none() {
            let path = target
                .strip_prefix(&self.root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| entry.name.clone());
            let message = format!("the date {:?} does not parse", entry.date);
            (self.progress)(BackupProgress::Warning {
                path: &path,
                message: &message,
            });
            self.report.date_warnings += 1;
            self.report.warnings.push(BackupError { path, message });
        }
        if let Ok(meta) = std::fs::symlink_metadata(target) {
            if !meta.is_file() {
                return Err("a folder or link is in the way".into());
            }
            // Kept only when the size and the date both match: a file changed on the device
            // since the earlier run is downloaded again.
            if self.options.resume
                && meta.len() == entry.size
                && mtime.is_some()
                && meta.modified().ok() == mtime
            {
                return Ok(true);
            }
        }
        let part = part_path(target, siblings);
        let mut last = String::new();
        for _ in 0..TRIES {
            match self.fetch(&entry.uri, &part, mtime) {
                Ok(n) if n == entry.size => {
                    std::fs::rename(&part, target).map_err(|e| {
                        let _ = std::fs::remove_file(&part);
                        format!("cannot rename the .part file: {e}")
                    })?;
                    return Ok(false);
                }
                Ok(n) => {
                    last = format!("received {n} bytes, the listing says {}", entry.size);
                }
                Err(e) => last = e,
            }
        }
        let _ = std::fs::remove_file(&part);
        Err(format!("failed after {TRIES} tries: {last}"))
    }

    /// One GET of `uri` into `part`; returns the byte count.
    fn fetch(
        &self,
        uri: &str,
        part: &Path,
        mtime: Option<SystemTime>,
    ) -> std::result::Result<u64, String> {
        let mut resp = self
            .client
            .get(self.url(uri))
            .send()
            .map_err(|e| e.to_string())?;
        if resp.status() != reqwest::StatusCode::OK {
            return Err(format!("the device answered {}", resp.status()));
        }
        let mut out = std::fs::File::create(part).map_err(|e| e.to_string())?;
        let n = resp.copy_to(&mut out).map_err(|e| e.to_string())?;
        if let Some(t) = mtime {
            out.set_modified(t).map_err(|e| e.to_string())?;
        }
        out.sync_all().map_err(|e| e.to_string())?;
        Ok(n)
    }
}

/// `<name>.part` next to `target`, or a short dot-name `.nd-backup-<8 hex>.part` when that
/// would be too long or is the name of another entry in the folder. A leftover of either kind
/// (a killed run) refuses the import, which refuses every `*.part`; `--resume` removes them.
fn part_path(target: &Path, siblings: &HashSet<String>) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dir = target.parent().unwrap_or(Path::new("."));
    let part = format!("{name}.part");
    if name.len() <= MAX_PART_NAME && !siblings.contains(&part) {
        dir.join(part)
    } else {
        dir.join(format!(
            ".nd-backup-{}.part",
            &crate::journal::new_op_id()[..8]
        ))
    }
}
