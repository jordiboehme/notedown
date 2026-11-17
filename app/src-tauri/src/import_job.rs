//! The backup of the Supernote (`cloud backup-device`) and the imports into the app's own
//! server (`cloud import`, `cloud import-app-data`), each with `--json`, for the Setup page.
//!
//! One job runs at a time, in a process group of its own, so Pause and Quit stop it with
//! everything it started; `--exit-with-parent` ends it when the app dies. Its stdout lines are
//! JSON: progress lines, then one final line (`report`, `dryRun`, `imported`, `appData`). A job
//! that printed its final line ends as [`JobEvent::Done`], also with exit code 1 (a backup or a
//! dry run with errors); one without a final line ends as [`JobEvent::Failed`] with its
//! `error:` line.
//!
//! A real import runs while the app's own server and pipeline are stopped: nothing else writes
//! to the account meanwhile. It holds the [`ServerUse`] lock from before the stop until the
//! children run again, so no command that starts them or changes the login runs during it.
//! They start again after it, also after a failure, but not when the app quits.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::commands::ConfigState;
use crate::config::{AppConfig, Mode, SyncMark};
use crate::poller::Poller;
use crate::process::{self, SpawnFn};
use crate::processes::Processes;
use crate::rest::Rest;
use crate::status::OUTSIDE_LINE;
use crate::supervisor::ChildState;
use crate::token::token_key;

pub const SETUP_JOB_PROGRESS_EVENT: &str = "setup-job-progress";
pub const SETUP_JOB_DONE_EVENT: &str = "setup-job-done";
pub const SETUP_JOB_FAILED_EVENT: &str = "setup-job-failed";
pub const SETUP_JOB_PAUSED_EVENT: &str = "setup-job-paused";

/// What a second step hears while one runs.
pub const ANOTHER_RUNS: &str = "Another step is running.";
/// What the commands that touch the server or the login hear while an import runs.
pub const WAIT_FOR_IMPORT: &str = "Wait until the import is done.";
/// A backup folder with files that are not the app's backup.
pub const NOT_EMPTY: &str = "The folder holds other files. Choose an empty folder.";
/// A backup folder with a backup that is done.
pub const BACKUP_DONE_THERE: &str = "The folder holds a finished backup. Choose an empty folder.";
/// A backup folder with a backup that did not finish, without Resume.
pub const BACKUP_NOT_DONE: &str =
    "The folder holds a backup that did not finish. Resume it or choose an empty folder.";
/// A backup folder that cannot be read.
pub const CANNOT_READ: &str = "Notedown cannot read this folder.";
/// The imports write to the server's data folder, which is on this Mac only in Local mode.
pub const IMPORT_HERE_ONLY: &str = "The import runs on the machine of the server.";
/// The phase of an import's progress while the server starts again.
pub const RESTARTING: &str = "restarting";

const QUITTING: &str = "Notedown is quitting.";

/// How long a stopped job may take to end before SIGKILL.
pub const JOB_GRACE: Duration = Duration::from_secs(5);
/// How long a stop waits for the job to be reaped after SIGKILL.
const GONE_WAIT: Duration = Duration::from_secs(5);
/// How long the end of a job waits for its last stdout and stderr lines.
const DRAIN: Duration = Duration::from_secs(1);
/// The stderr lines a job keeps for its error text.
const STDERR_LINES: usize = 50;
/// How long one `GET /events` for the mark of a server on another machine may take.
const MARK_TIMEOUT: Duration = Duration::from_secs(8);
/// How much of the end of a journal file is read at first to find its last line.
const TAIL: u64 = 64 * 1024;

/// The steps of the setup that run the sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JobKind {
    Backup,
    ImportDryRun,
    Import,
    AppDataDryRun,
    AppData,
}

impl JobKind {
    /// The key of the job's final line.
    fn final_key(self) -> &'static str {
        match self {
            JobKind::Backup => "report",
            JobKind::ImportDryRun => "dryRun",
            JobKind::Import => "imported",
            JobKind::AppDataDryRun | JobKind::AppData => "appData",
        }
    }

    /// The job writes to the account: the app's own server and pipeline stop for it.
    fn holds_server(self) -> bool {
        matches!(self, JobKind::Import | JobKind::AppData)
    }
}

/// One progress line: a backup counts `files` and `bytes`, an import names its `phase` with
/// `files` (recycling) or `done` of `total` (importing). The app adds the phase
/// [`RESTARTING`] while the server starts again after an import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobProgress {
    pub kind: JobKind,
    pub files: Option<u64>,
    pub bytes: Option<u64>,
    pub done: Option<u64>,
    pub total: Option<u64>,
    pub phase: Option<String>,
}

/// How a job with a final line ended. `json` is the content of the final line; when `ok` is
/// false (exit code 1) it also holds the job's error text as `error`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobResult {
    pub kind: JobKind,
    pub ok: bool,
    pub json: serde_json::Value,
}

/// What a job tells the app.
#[derive(Debug, Clone, PartialEq)]
pub enum JobEvent {
    Progress(JobProgress),
    Done(JobResult),
    /// The job ended without a final line; the text is its error.
    Failed {
        kind: JobKind,
        text: String,
    },
    /// Pause stopped the job, and the next step may start.
    Paused {
        kind: JobKind,
    },
}

/// Gets every event. It must not block: the app only sends it into a channel.
pub type Sink = Arc<dyn Fn(&JobEvent) + Send + Sync>;

/// What a job needs of the rest of the app: the app's own children and the poller, or fakes
/// in the tests.
pub trait Host: Send + Sync {
    fn config(&self) -> AppConfig;
    /// The server or the pipeline runs outside the app.
    fn outside(&self) -> bool;
    /// Stops the app's server and pipeline (before an import).
    fn stop_all(&self);
    /// Starts them again as the config asks (after an import).
    fn start(&self);
    fn poll_now(&self);
    /// The files are imported into this Mac's server, before it starts again: takes the sync
    /// mark from the journal.
    fn files_imported(&self);
    /// The app data is imported too: the backup choices are no longer needed.
    fn app_data_imported(&self);
}

/// Who uses the app's server: the commands that start the children or change the login use
/// it ([`ServerUse::try_use`]), each for its whole run, and an import holds it alone
/// ([`ServerUse::try_hold`]). Neither waits: the one that comes second is refused. Cheap to
/// clone; every clone is the same lock.
#[derive(Clone, Default)]
pub struct ServerUse(Arc<Mutex<Users>>);

#[derive(Default)]
struct Users {
    using: usize,
    held: bool,
}

/// A command's use of the server; it ends when the guard is dropped.
#[must_use]
pub struct UseGuard(ServerUse);

/// An import's hold of the server; it ends when the guard is dropped.
#[must_use]
struct HoldGuard(ServerUse);

impl ServerUse {
    /// Refused with [`WAIT_FOR_IMPORT`] while an import holds the server.
    pub fn try_use(&self) -> Result<UseGuard, String> {
        let mut users = lock(&self.0);
        if users.held {
            return Err(WAIT_FOR_IMPORT.to_owned());
        }
        users.using += 1;
        Ok(UseGuard(self.clone()))
    }

    /// `None` while a command uses the server or another import holds it.
    fn try_hold(&self) -> Option<HoldGuard> {
        let mut users = lock(&self.0);
        if users.held || users.using > 0 {
            return None;
        }
        users.held = true;
        Some(HoldGuard(self.clone()))
    }
}

impl Drop for UseGuard {
    fn drop(&mut self) {
        let mut users = lock(&(self.0).0);
        users.using = users.using.saturating_sub(1);
    }
}

impl Drop for HoldGuard {
    fn drop(&mut self) {
        lock(&(self.0).0).held = false;
    }
}

/// What a backup folder holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "state")]
pub enum BackupFolder {
    Missing,
    Empty,
    /// An earlier backup that did not finish; `time` is when it started.
    Resumable {
        time: String,
    },
    /// A backup that finished at `time`.
    Complete {
        time: String,
    },
    /// Anything else: no backup of the app's.
    Foreign,
    /// The folder (or its `backup.json`) cannot be read.
    Unreadable,
}

/// The fields of `backup.json` the app reads.
#[derive(Deserialize)]
struct BackupFile {
    time: String,
    complete: bool,
}

/// What `dir` holds: a folder with `backup.json` is the app's backup, done or not; any other
/// entry (also `.DS_Store`, which `backup-device` refuses as well) makes it [`Foreign`].
///
/// [`Foreign`]: BackupFolder::Foreign
pub fn backup_folder_state(dir: &Path) -> BackupFolder {
    let mut entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return BackupFolder::Missing,
        // A file is no folder for a backup.
        Err(_) if dir.is_file() => return BackupFolder::Foreign,
        Err(_) => return BackupFolder::Unreadable,
    };
    match entries.next() {
        None => return BackupFolder::Empty,
        Some(Err(_)) => return BackupFolder::Unreadable,
        Some(Ok(_)) => {}
    }
    let text = match std::fs::read(dir.join("backup.json")) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return BackupFolder::Foreign,
        Err(_) => return BackupFolder::Unreadable,
    };
    match serde_json::from_slice::<BackupFile>(&text) {
        Ok(file) if file.complete => BackupFolder::Complete { time: file.time },
        Ok(file) => BackupFolder::Resumable { time: file.time },
        Err(_) => BackupFolder::Foreign,
    }
}

/// The highest journal number of the server, by the token of an account
/// ([`crate::setup::journal_end`]), within a short bound (the mark of a server on another
/// machine and the end of the check of the first sync).
pub fn events_end(base: &str, token: crate::token::Token) -> Result<u64, String> {
    let rest = Rest::with_timeout(base, Some(token), MARK_TIMEOUT).map_err(|e| e.to_string())?;
    crate::setup::journal_end(&rest).map_err(|e| e.to_string())
}

/// The `seq` of a journal line.
#[derive(Deserialize)]
struct Seq {
    seq: u64,
}

/// The last journal number on disk: the `seq` of the last complete line of the newest
/// `<data>/journal/journal-<first seq>.jsonl` that has one. A last line without its newline
/// is an append that did not finish and does not count. `None` without a journal.
pub fn journal_end(data_dir: &Path) -> Option<u64> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(data_dir.join("journal"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("journal-") && n.ends_with(".jsonl"))
        })
        .collect();
    // The first seq has 12 digits: the names sort as the files follow one another.
    files.sort();
    files.iter().rev().find_map(|file| last_seq(file))
}

/// The `seq` of the last complete line of `file`, read from its end.
fn last_seq(file: &Path) -> Option<u64> {
    let mut f = std::fs::File::open(file).ok()?;
    let len = f.metadata().ok()?.len();
    let mut window = TAIL;
    loop {
        let start = len.saturating_sub(window);
        f.seek(SeekFrom::Start(start)).ok()?;
        let mut tail = Vec::new();
        f.by_ref().take(len - start).read_to_end(&mut tail).ok()?;
        // Up to the last newline: what follows it is a torn line. A torn line longer than the
        // window hides the newline before it: read more.
        let Some(end) = tail.iter().rposition(|b| *b == b'\n') else {
            if start == 0 {
                return None;
            }
            window = window.saturating_mul(2);
            continue;
        };
        let complete = &tail[..end];
        match complete.iter().rposition(|b| *b == b'\n') {
            Some(at) => return line_seq(&complete[at + 1..]),
            None if start == 0 => return line_seq(complete),
            // The last line is longer than the window: read more.
            None => window = window.saturating_mul(2),
        }
    }
}

fn line_seq(line: &[u8]) -> Option<u64> {
    serde_json::from_slice::<Seq>(line).ok().map(|s| s.seq)
}

/// Takes the sync mark of the logged-in account of this Mac's server from its journal on
/// disk: `Ok(true)` when it was saved, `Ok(false)` without an account or a journal.
pub fn take_journal_mark(state: &ConfigState) -> Result<bool, String> {
    let config = state.current();
    let Some(key) = token_key(&config) else {
        return Ok(false);
    };
    let Some(seq) = journal_end(&config.data_dir) else {
        return Ok(false);
    };
    let sync_at_mark = config
        .account_slug()
        .and_then(|slug| newest_device_sync(&config.data_dir, slug));
    state.set_sync_mark(SyncMark {
        key,
        seq,
        at_ms: now_ms(),
        sync_at_mark,
    })
}

/// The wall clock in milliseconds since 1970.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The `last_sync_ms` of a device file, as far as the mark reads it.
#[derive(Deserialize)]
struct DeviceFile {
    #[serde(default)]
    last_sync_ms: Option<u64>,
}

/// The newest `last_sync_ms` (the server's clock) of the devices of the account `slug` in
/// `<data>/accounts/<slug>/devices/*.json`; `None` when none synced. A file that cannot be read
/// is left out.
pub fn newest_device_sync(data_dir: &Path, slug: &str) -> Option<u64> {
    let dir = data_dir.join("accounts").join(slug).join("devices");
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| std::fs::read(p).ok())
        .filter_map(|text| serde_json::from_slice::<DeviceFile>(&text).ok())
        .filter_map(|d| d.last_sync_ms)
        .max()
}

/// Whether a final report says the step went well, as its exit code would: a backup without
/// errors and without stale files. Only a backup can be paused; any other kind is not clean.
fn report_clean(kind: JobKind, json: &serde_json::Value) -> bool {
    let empty = |key: &str| {
        json.get(key)
            .and_then(serde_json::Value::as_array)
            .is_none_or(Vec::is_empty)
    };
    kind == JobKind::Backup && empty("errors") && empty("stale")
}

/// The newest device sync the import's guard compares with: the larger of what the status
/// knows (`from_status`) and the device files of the account on disk. The status has none while
/// this Mac's server is not running; the files on disk are the truth then (imports run only in
/// Local mode).
pub fn last_device_sync(config: &AppConfig, from_status: Option<u64>) -> Option<u64> {
    let disk = config
        .account_slug()
        .and_then(|slug| newest_device_sync(&config.data_dir, slug));
    from_status.max(disk)
}

/// What the import refuses for a backup older than 24 hours or not newer than the last sync.
pub const BACKUP_TOO_OLD: &str = "This backup is too old. Make a new backup.";
/// What the app data import refuses for a `.snbak` file older than 24 hours.
pub const SNBAK_TOO_OLD: &str = "This .snbak file is too old. Make a new one on the Supernote.";
/// How old a backup or a `.snbak` file may be.
const FRESH_MS: u64 = 24 * 3_600_000;

/// The check after the server stopped: the backup in `dir` was made after the newest device
/// sync `last_sync_ms`. A time that cannot be read passes here; [`backup_fresh`] refused it
/// before the stop.
pub fn backup_synced_before(dir: &Path, last_sync_ms: Option<u64>) -> Result<(), String> {
    let time = std::fs::read(dir.join("backup.json"))
        .ok()
        .and_then(|text| serde_json::from_slice::<BackupFile>(&text).ok())
        .and_then(|file| rfc3339_ms(&file.time));
    match (time, last_sync_ms) {
        (Some(made), Some(sync)) if made <= sync => Err(BACKUP_TOO_OLD.to_owned()),
        _ => Ok(()),
    }
}

/// The import's own guard (the Setup page checks this as well): the backup in `dir` was made
/// within 24 hours of `now_ms` and after the newest device sync `last_sync_ms` (a file missing
/// on the server is deleted on the Supernote). A backup whose time cannot be read is refused.
pub fn backup_fresh(dir: &Path, now_ms: u64, last_sync_ms: Option<u64>) -> Result<(), String> {
    let time = std::fs::read(dir.join("backup.json"))
        .ok()
        .and_then(|text| serde_json::from_slice::<BackupFile>(&text).ok())
        .and_then(|file| rfc3339_ms(&file.time));
    let fresh = time.is_some_and(|made| {
        now_ms.saturating_sub(made) <= FRESH_MS && last_sync_ms.is_none_or(|sync| made > sync)
    });
    if fresh {
        Ok(())
    } else {
        Err(BACKUP_TOO_OLD.to_owned())
    }
}

/// The app data import's own guard: the `.snbak` file was written within 24 hours of `now_ms`.
pub fn snbak_fresh(path: &Path, now_ms: u64) -> Result<(), String> {
    let written = crate::commands::file_time_ms(path)?;
    if now_ms.saturating_sub(written) <= FRESH_MS {
        Ok(())
    } else {
        Err(SNBAK_TOO_OLD.to_owned())
    }
}

/// Milliseconds since 1970 of an RFC 3339 time (`backup.json`'s `time`): `YYYY-MM-DDTHH:MM:SS`,
/// an optional fraction, and `Z` or `+HH:MM` / `-HH:MM`.
pub fn rfc3339_ms(text: &str) -> Option<u64> {
    let b = text.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> {
        let part = text.get(from..to)?;
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, minute, second) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    if second > 60 {
        return None;
    }
    let mut rest = &text[19..];
    let mut millis: i64 = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let shown = &fraction[..digits.min(3)];
        millis = shown.parse::<i64>().ok()? * 10_i64.pow(3 - u32::try_from(shown.len()).ok()?);
        rest = &fraction[digits..];
    }
    let offset_s: i64 = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            if rest.len() != 6 || rest.as_bytes()[3] != b':' {
                return None;
            }
            let h: i64 = rest[1..3].parse().ok()?;
            let m: i64 = rest[4..6].parse().ok()?;
            sign * (h * 3600 + m * 60)
        }
    };
    // Days from 1970-01-01 of the civil date (Howard Hinnant's days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset_s;
    u64::try_from(seconds * 1000 + millis).ok()
}

/// What a job that failed says: its last `error:` line (the CLI prints one), else its last
/// stderr line, else how it ended. Warnings and other lines before it are left out.
pub fn job_error_text(stderr: &[u8], status: ExitStatus) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if let Some(error) = lines.iter().rev().find_map(|l| l.strip_prefix("error: ")) {
        return error.to_owned();
    }
    if let Some(last) = lines.last() {
        return (*last).to_owned();
    }
    match status.code() {
        Some(code) => format!("it ended with exit code {code}"),
        None => "it ended by a signal".to_owned(),
    }
}

/// The backup and the imports. Cheap to clone; every clone is the same control.
#[derive(Clone)]
pub struct ImportJobs {
    inner: Arc<Inner>,
}

struct Inner {
    spawn: SpawnFn,
    grace: Duration,
    sink: Sink,
    host: Arc<dyn Host>,
    server: ServerUse,
    run: Mutex<Run>,
    /// Signals the end of a job's process.
    reaped: Condvar,
    /// A wait between the end of the process and its reap, so a test can pause in it.
    #[cfg(test)]
    after_exit: Mutex<Duration>,
}

#[derive(Default)]
struct Run {
    /// The running job, from its start (an import: before the server stops) to its event.
    kind: Option<JobKind>,
    /// The process group of the running job, until it ended. Its leader is not reaped while
    /// this is set, so the number cannot belong to another process.
    pgid: Option<u32>,
    /// Pause or Quit stopped the job: its end is no failure.
    stop_requested: bool,
    /// After Quit nothing starts.
    quitting: bool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Waits until the process `pid` (a child of this one) has ended, without reaping it.
fn wait_ended(pid: u32) {
    let pid: libc::id_t = pid;
    loop {
        // SAFETY: an all-zero siginfo_t is a valid value; waitid only writes to it.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waitid with WNOWAIT only reads the state of our own child.
        let r = unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) };
        if r == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return;
        }
    }
}

/// Whether the child `pid` has ended (and is not reaped yet).
fn has_ended(pid: u32) -> bool {
    let id: libc::id_t = pid;
    // SAFETY: an all-zero siginfo_t is a valid value; waitid only writes to it.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: as in [`wait_ended`]; WNOHANG makes it answer at once.
    let r = unsafe {
        libc::waitid(
            libc::P_PID,
            id,
            &mut info,
            libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
        )
    };
    r == 0 && info.si_pid != 0
}

impl ImportJobs {
    /// `grace` is the wait between SIGTERM and SIGKILL when a job is stopped.
    pub fn new(spawn: SpawnFn, grace: Duration, sink: Sink, host: Arc<dyn Host>) -> Self {
        Self {
            inner: Arc::new(Inner {
                spawn,
                grace,
                sink,
                host,
                server: ServerUse::default(),
                run: Mutex::new(Run::default()),
                reaped: Condvar::new(),
                #[cfg(test)]
                after_exit: Mutex::new(Duration::ZERO),
            }),
        }
    }

    /// Backs the Supernote at `url` up into `dir`: a missing or empty folder, or with `resume`
    /// one that holds a backup that did not finish. Any other folder is refused before the
    /// sidecar runs.
    pub fn backup(&self, url: &str, dir: &Path, resume: bool) -> Result<(), String> {
        match (backup_folder_state(dir), resume) {
            (BackupFolder::Missing | BackupFolder::Empty, _)
            | (BackupFolder::Resumable { .. }, true) => {}
            (BackupFolder::Resumable { .. }, false) => return Err(BACKUP_NOT_DONE.to_owned()),
            (BackupFolder::Complete { .. }, _) => return Err(BACKUP_DONE_THERE.to_owned()),
            (BackupFolder::Foreign, _) => return Err(NOT_EMPTY.to_owned()),
            (BackupFolder::Unreadable, _) => return Err(CANNOT_READ.to_owned()),
        }
        let mut args = vec![
            "cloud".to_owned(),
            "backup-device".to_owned(),
            url.trim().to_owned(),
            text(dir),
        ];
        if resume {
            args.push("--resume".to_owned());
        }
        self.inner.start(JobKind::Backup, || Ok(args))
    }

    /// Stops a running backup; its `.part` files stay for a resume. Returns when it is gone;
    /// [`JobEvent::Paused`] comes once the next step may start. Nothing else is stopped by it,
    /// and a backup that ended by itself gives its result.
    pub fn pause(&self) {
        self.inner.stop(Some(JobKind::Backup));
    }

    /// Stops the running job for good: nothing starts after it, and an import does not start
    /// the server again (the app quits).
    pub fn quit(&self) {
        lock(&self.inner.run).quitting = true;
        self.inner.stop(None);
    }

    /// `cloud import --dry-run` of the backup in `backup_dir`.
    pub fn import_dry_run(
        &self,
        backup_dir: &Path,
        replace: bool,
        allow_empty: bool,
    ) -> Result<(), String> {
        self.inner.start(JobKind::ImportDryRun, || {
            self.import_args(backup_dir, true, replace, allow_empty)
        })
    }

    /// `cloud import` of the backup in `backup_dir`, with the server stopped.
    pub fn import(
        &self,
        backup_dir: &Path,
        replace: bool,
        allow_empty: bool,
    ) -> Result<(), String> {
        self.inner.start_checked(
            JobKind::Import,
            || self.import_args(backup_dir, false, replace, allow_empty),
            // The server is stopped now and no device sync can end any more: a sync that
            // ended while it stopped is on disk and is checked against the backup again.
            || {
                let config = self.inner.host.config();
                backup_synced_before(backup_dir, last_device_sync(&config, None))
            },
        )
    }

    /// `cloud import-app-data --dry-run` of `snbak`; `None` is `--empty`.
    pub fn app_data_dry_run(&self, snbak: Option<&Path>, replace: bool) -> Result<(), String> {
        self.inner.start(JobKind::AppDataDryRun, || {
            self.app_data_args(snbak, true, replace)
        })
    }

    /// `cloud import-app-data` of `snbak` (`None` is `--empty`), with the server stopped.
    pub fn app_data(&self, snbak: Option<&Path>, replace: bool) -> Result<(), String> {
        self.inner.start(JobKind::AppData, || {
            self.app_data_args(snbak, false, replace)
        })
    }

    /// A job runs.
    pub fn busy(&self) -> bool {
        lock(&self.inner.run).kind.is_some()
    }

    /// The running job.
    pub fn state(&self) -> Option<JobKind> {
        lock(&self.inner.run).kind
    }

    /// The guard of the commands that start the children or change the login, held for the
    /// whole command: refused with [`WAIT_FOR_IMPORT`] while an import holds the server, and
    /// an import does not start while one is held.
    pub fn use_server(&self) -> Result<UseGuard, String> {
        self.inner.server.try_use()
    }

    /// The lock behind [`use_server`](Self::use_server).
    pub fn server_use(&self) -> ServerUse {
        self.inner.server.clone()
    }

    /// `--data <dir> --account <slug>` of the app's own server: Local mode with an account.
    fn data_args(&self) -> Result<Vec<String>, String> {
        let config = self.inner.host.config();
        if config.mode != Mode::Local {
            return Err(IMPORT_HERE_ONLY.to_owned());
        }
        let account = config
            .account_slug()
            .ok_or_else(|| crate::setup::LOG_IN_FIRST.to_owned())?;
        Ok(vec![
            "--data".to_owned(),
            text(&config.data_dir),
            "--account".to_owned(),
            account.to_owned(),
        ])
    }

    fn import_args(
        &self,
        backup_dir: &Path,
        dry_run: bool,
        replace: bool,
        allow_empty: bool,
    ) -> Result<Vec<String>, String> {
        let mut args = vec!["cloud".to_owned(), "import".to_owned(), text(backup_dir)];
        args.extend(self.data_args()?);
        for (on, flag) in [
            (dry_run, "--dry-run"),
            (replace, "--replace"),
            (allow_empty, "--allow-empty"),
        ] {
            if on {
                args.push(flag.to_owned());
            }
        }
        Ok(args)
    }

    fn app_data_args(
        &self,
        snbak: Option<&Path>,
        dry_run: bool,
        replace: bool,
    ) -> Result<Vec<String>, String> {
        let mut args = vec![
            "cloud".to_owned(),
            "import-app-data".to_owned(),
            snbak.map_or_else(|| "--empty".to_owned(), text),
        ];
        args.extend(self.data_args()?);
        for (on, flag) in [(dry_run, "--dry-run"), (replace, "--replace")] {
            if on {
                args.push(flag.to_owned());
            }
        }
        Ok(args)
    }

    /// Makes the end of a job wait `wait` between the end of its process and its reap.
    #[cfg(test)]
    fn hold_after_exit(&self, wait: Duration) {
        *lock(&self.inner.after_exit) = wait;
    }
}

impl Inner {
    /// Starts `kind` with the arguments of `args` and `--json --exit-with-parent <pid>`. An
    /// import takes the server first (refused while a command uses it), then reads its
    /// arguments (the config cannot change meanwhile) and stops the app's server and pipeline;
    /// it is refused while either runs outside the app.
    fn start(
        self: &Arc<Self>,
        kind: JobKind,
        args: impl FnOnce() -> Result<Vec<String>, String>,
    ) -> Result<(), String> {
        self.start_checked(kind, args, || Ok(()))
    }

    /// [`start`](Self::start) with a last check `after_stop`, made once the app's server is
    /// stopped and before the program starts. When it refuses, the server starts again.
    fn start_checked(
        self: &Arc<Self>,
        kind: JobKind,
        args: impl FnOnce() -> Result<Vec<String>, String>,
        after_stop: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        let holds = kind.holds_server();
        if holds && self.host.outside() {
            return Err(OUTSIDE_LINE.to_owned());
        }
        let hold = {
            let mut run = lock(&self.run);
            if run.quitting {
                return Err(QUITTING.to_owned());
            }
            if run.kind.is_some() {
                return Err(ANOTHER_RUNS.to_owned());
            }
            let hold = if holds {
                Some(
                    self.server
                        .try_hold()
                        .ok_or_else(|| ANOTHER_RUNS.to_owned())?,
                )
            } else {
                None
            };
            // Taken before the stop, which can take the children's grace: no second job
            // gets in meanwhile.
            run.kind = Some(kind);
            run.stop_requested = false;
            hold
        };
        let mut args = match args() {
            Ok(args) => args,
            Err(why) => {
                lock(&self.run).kind = None;
                return Err(why);
            }
        };
        if holds {
            self.host.stop_all();
            if let Err(why) = after_stop() {
                // No "restarting" progress: the command returns the error.
                self.start_children();
                lock(&self.run).kind = None;
                drop(hold);
                return Err(why);
            }
        }
        // The job also ends when the app dies without a clean Quit, as the children do.
        args.extend([
            "--json".to_owned(),
            "--exit-with-parent".to_owned(),
            std::process::id().to_string(),
        ]);
        let mut command = (self.spawn)(&args);
        command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut run = lock(&self.run);
        // A Quit during the stop: nothing runs and nothing starts again.
        if run.quitting {
            run.kind = None;
            return Err(QUITTING.to_owned());
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                drop(run);
                // No "restarting" progress: the command returns the error, and a progress event
                // that arrived after it would show the step as running.
                if holds {
                    self.start_children();
                }
                lock(&self.run).kind = None;
                drop(hold);
                return Err(format!("The notedown program could not start: {e}"));
            }
        };
        run.pgid = Some(child.id());
        drop(run);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let inner = self.clone();
        std::thread::spawn(move || inner.follow(kind, child, stdout, stderr, hold));
        Ok(())
    }

    fn quitting(&self) -> bool {
        lock(&self.run).quitting
    }

    /// Starts the app's server and pipeline again after an import and polls; the Setup page
    /// sees the "restarting" phase.
    fn restart(&self, kind: JobKind) {
        (self.sink)(&JobEvent::Progress(JobProgress {
            kind,
            files: None,
            bytes: None,
            done: None,
            total: None,
            phase: Some(RESTARTING.to_owned()),
        }));
        self.start_children();
    }

    /// Starts the app's server and pipeline again and polls, without a progress event.
    fn start_children(&self) {
        self.host.start();
        self.host.poll_now();
    }

    /// Reads the job's lines until it ends, takes the mark and starts the server again after
    /// an import, and reports how the job ended.
    fn follow(
        self: Arc<Self>,
        kind: JobKind,
        mut child: Child,
        stdout: Option<ChildStdout>,
        stderr: Option<ChildStderr>,
        hold: Option<HoldGuard>,
    ) {
        let last = Arc::new(Mutex::new(None::<serde_json::Value>));
        let errors = Arc::new(Mutex::new(Vec::<u8>::new()));
        let (done_tx, done_rx) = mpsc::channel::<()>();
        if let Some(stdout) = stdout {
            let inner = self.clone();
            let last = last.clone();
            let done = done_tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    inner.read_line(kind, &line, &last);
                }
                drop(done);
            });
        }
        if let Some(stderr) = stderr {
            let errors = errors.clone();
            let done = done_tx.clone();
            std::thread::spawn(move || {
                let mut lines: VecDeque<String> = VecDeque::new();
                for line in BufReader::new(stderr).lines() {
                    let Ok(line) = line else { break };
                    lines.push_back(line);
                    if lines.len() > STDERR_LINES {
                        lines.pop_front();
                    }
                    let mut text = lines.iter().cloned().collect::<Vec<_>>().join("\n");
                    text.push('\n');
                    *lock(&errors) = text.into_bytes();
                }
                drop(done);
            });
        }
        drop(done_tx);
        // Ended, not reaped: until `pgid` is cleared a stop sees that it ended by itself, and
        // its number still belongs to it.
        wait_ended(child.id());
        #[cfg(test)]
        std::thread::sleep(*lock(&self.after_exit));
        {
            let mut run = lock(&self.run);
            run.pgid = None;
            self.reaped.notify_all();
        }
        let status = child.wait();
        // The readers end with the pipes; a grandchild may hold them, so the wait is bounded.
        let until = Instant::now() + DRAIN;
        while let Err(mpsc::RecvTimeoutError::Timeout) =
            done_rx.recv_timeout(until.saturating_duration_since(Instant::now()))
        {
            if Instant::now() >= until {
                break;
            }
        }
        let stopped = lock(&self.run).stop_requested;
        // A final line that was already read wins over a pause: the step had ended.
        let final_read = lock(&last).is_some();
        let event = match (status, lock(&last).take()) {
            (Ok(status), Some(mut json)) => {
                // Ended by the pause's signal after its report: the report says how it ended,
                // as the exit code would have.
                let ok = if stopped && status.code().is_none() {
                    report_clean(kind, &json)
                } else {
                    status.success()
                };
                if !ok && let Some(object) = json.as_object_mut() {
                    let error = job_error_text(&lock(&errors), status);
                    object.insert("error".to_owned(), serde_json::Value::String(error));
                }
                JobEvent::Done(JobResult { kind, ok, json })
            }
            (Ok(status), None) => JobEvent::Failed {
                kind,
                text: job_error_text(&lock(&errors), status),
            },
            (Err(e), _) => JobEvent::Failed {
                kind,
                text: format!("the step could not be followed: {e}"),
            },
        };
        // The server starts again, also after a failure; never when the app quits. The mark
        // is the journal's end while the server is still stopped: no device change is in it.
        if kind.holds_server() && !self.quitting() {
            if matches!(&event, JobEvent::Done(r) if r.ok) {
                match kind {
                    JobKind::Import => self.host.files_imported(),
                    JobKind::AppData => self.host.app_data_imported(),
                    _ => {}
                }
            }
            self.restart(kind);
        }
        drop(hold);
        let quitting = {
            let mut run = lock(&self.run);
            run.kind = None;
            run.quitting
        };
        if !stopped || (final_read && !quitting) {
            (self.sink)(&event);
        } else if !quitting {
            (self.sink)(&JobEvent::Paused { kind });
        }
    }

    fn read_line(&self, kind: JobKind, line: &str, last: &Mutex<Option<serde_json::Value>>) {
        let Ok(serde_json::Value::Object(mut object)) = serde_json::from_str(line) else {
            return;
        };
        if let Some(json) = object.remove(kind.final_key()) {
            *lock(last) = Some(json);
            return;
        }
        let number = |key: &str| object.get(key).and_then(serde_json::Value::as_u64);
        let progress = JobProgress {
            kind,
            files: number("files"),
            bytes: number("bytes"),
            done: number("done"),
            total: number("total"),
            phase: object
                .get("phase")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        };
        if progress.files.is_none()
            && progress.bytes.is_none()
            && progress.done.is_none()
            && progress.phase.is_none()
        {
            return;
        }
        if lock(&self.run).stop_requested {
            // Stopped: what it says while it ends is no progress.
            return;
        }
        (self.sink)(&JobEvent::Progress(progress));
    }

    /// SIGTERM to the job's group, SIGKILL after the grace; returns when it ended. With
    /// `only`, a job of another kind runs on. A job that ended by itself is not stopped: it
    /// gives its result.
    fn stop(&self, only: Option<JobKind>) {
        let mut run = lock(&self.run);
        let Some(pgid) = run.pgid else {
            return;
        };
        if only.is_some() && run.kind != only {
            return;
        }
        if has_ended(pgid) {
            return;
        }
        run.stop_requested = true;
        let _ = process::signal_group(pgid, libc::SIGTERM);
        let (mut run, timeout) = self
            .reaped
            .wait_timeout_while(run, self.grace, |r| r.pgid.is_some())
            .unwrap_or_else(PoisonError::into_inner);
        if timeout.timed_out() && run.pgid.is_some() {
            let _ = process::signal_group(pgid, libc::SIGKILL);
            run = self
                .reaped
                .wait_timeout_while(run, GONE_WAIT, |r| r.pgid.is_some())
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        // No signal after the reap: the group's number may belong to someone else by then.
        drop(run);
    }
}

/// The app's own children, the poller and the config.
pub struct AppHost {
    pub config: ConfigState,
    pub processes: Arc<Processes>,
    pub poller: Poller,
}

impl Host for AppHost {
    fn config(&self) -> AppConfig {
        self.config.current()
    }

    fn outside(&self) -> bool {
        let states = self.processes.states();
        [states.server, states.pipeline]
            .iter()
            .any(|s| matches!(s, ChildState::Outside { .. }))
    }

    fn stop_all(&self) {
        self.processes.stop_all();
    }

    fn start(&self) {
        let reader = self.config.reader();
        self.processes.start_with(|| reader.current());
    }

    fn poll_now(&self) {
        self.poller.poll_now();
    }

    fn files_imported(&self) {
        if let Err(why) = take_journal_mark(&self.config) {
            eprintln!("notedown: the import mark was not saved: {why}");
        }
    }

    fn app_data_imported(&self) {
        if let Err(why) = self.config.imports_done() {
            eprintln!("notedown: the backup choices were not cleared: {why}");
        }
    }
}

#[cfg(test)]
mod tests;
