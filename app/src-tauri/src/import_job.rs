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
//! to the account meanwhile. They start again after it, also after a failure, but not when the
//! app quits.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdout, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::commands::ConfigState;
use crate::config::{AppConfig, Mode, SyncMark};
use crate::poller::{self, Poller};
use crate::process::{self, SpawnFn};
use crate::processes::Processes;
use crate::rest::Rest;
use crate::status::OUTSIDE_LINE;
use crate::supervisor::ChildState;
use crate::token::{TokenStore, token_key};

pub const SETUP_JOB_PROGRESS_EVENT: &str = "setup-job-progress";
pub const SETUP_JOB_DONE_EVENT: &str = "setup-job-done";
pub const SETUP_JOB_FAILED_EVENT: &str = "setup-job-failed";

/// What a second step hears while one runs.
pub const ANOTHER_RUNS: &str = "Another step is running.";
/// What the commands that touch the server or the login hear while a step runs.
pub const WAIT_FOR_IMPORT: &str = "Wait until the import is done.";
/// A backup folder with files that are not the app's backup.
pub const NOT_EMPTY: &str = "The folder holds other files. Choose an empty folder.";
/// A backup folder with a backup that is done.
pub const BACKUP_DONE_THERE: &str = "The folder holds a finished backup. Choose an empty folder.";
/// A backup folder with a backup that did not finish, without Resume.
pub const BACKUP_NOT_DONE: &str =
    "The folder holds a backup that did not finish. Resume it or choose an empty folder.";
/// The imports write to the server's data folder, which is on this Mac only in Local mode.
pub const IMPORT_HERE_ONLY: &str = "The import runs on the machine of the server.";

const QUITTING: &str = "Notedown is quitting.";

/// How long a stopped job may take to end before SIGKILL.
pub const JOB_GRACE: Duration = Duration::from_secs(5);
/// How long a stop waits for the job to be reaped after SIGKILL.
const GONE_WAIT: Duration = Duration::from_secs(5);
/// How long the end of a job waits for its last stdout and stderr lines.
const DRAIN: Duration = Duration::from_secs(1);
/// The stderr lines a job keeps for its error text.
const STDERR_LINES: usize = 50;
/// How long the mark waits for the server that starts again after the import.
const MARK_WAIT: Duration = Duration::from_secs(60);
/// The time between two tries of the mark.
const MARK_EVERY: Duration = Duration::from_millis(500);
/// How long one `GET /events` for the mark may take.
const MARK_TIMEOUT: Duration = Duration::from_secs(8);

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
/// `files` (recycling) or `done` of `total` (importing).
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
    /// The app data of this Mac's server is imported, after the restart: takes the sync mark
    /// and clears the backup choices. Gives up when `quitting` says so.
    fn imported(&self, quitting: &dyn Fn() -> bool);
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
        Err(_) => return BackupFolder::Foreign,
    };
    if entries.next().is_none() {
        return BackupFolder::Empty;
    }
    let Ok(text) = std::fs::read(dir.join("backup.json")) else {
        return BackupFolder::Foreign;
    };
    match serde_json::from_slice::<BackupFile>(&text) {
        Ok(file) if file.complete => BackupFolder::Complete { time: file.time },
        Ok(file) => BackupFolder::Resumable { time: file.time },
        Err(_) => BackupFolder::Foreign,
    }
}

/// The `end` of `GET /events`: the highest journal number.
#[derive(Deserialize)]
struct EventsEnd {
    end: u64,
}

/// The highest journal number of the account the token belongs to, from
/// `GET <base>/events?from=0&limit=1`.
pub fn events_end(base: &str, token: crate::token::Token) -> Result<u64, String> {
    let rest = Rest::with_timeout(base, Some(token), MARK_TIMEOUT).map_err(|e| e.to_string())?;
    rest.get_json::<EventsEnd>("/events", &[("from", "0"), ("limit", "1")])
        .map(|page| page.end)
        .map_err(|e| e.to_string())
}

/// Takes the sync mark of the logged-in account from the server at `base` once: `Ok(true)`
/// when it was saved, `Ok(false)` when there is no account, no token or a mark already.
pub fn mark_once(state: &ConfigState, base: &str, store: &dyn TokenStore) -> Result<bool, String> {
    let config = state.current();
    let Some(key) = token_key(&config) else {
        return Ok(false);
    };
    if config
        .setup
        .sync_mark
        .as_ref()
        .is_some_and(|m| m.key == key)
    {
        return Ok(false);
    }
    let Some(token) = store.get(&key).map_err(|e| e.to_string())? else {
        return Ok(false);
    };
    let seq = events_end(base, token)?;
    state.take_sync_mark(SyncMark { key, seq })
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
    run: Mutex<Run>,
    /// Signals the end of a job's process.
    reaped: Condvar,
}

#[derive(Default)]
struct Run {
    /// The running job, from its start (an import: before the server stops) to its event.
    kind: Option<JobKind>,
    /// The process group of the running job, until it is reaped.
    pgid: Option<u32>,
    /// Pause or Quit asked the job to stop: its end is no failure.
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

impl ImportJobs {
    /// `grace` is the wait between SIGTERM and SIGKILL when a job is stopped.
    pub fn new(spawn: SpawnFn, grace: Duration, sink: Sink, host: Arc<dyn Host>) -> Self {
        Self {
            inner: Arc::new(Inner {
                spawn,
                grace,
                sink,
                host,
                run: Mutex::new(Run::default()),
                reaped: Condvar::new(),
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
        self.inner.start(JobKind::Backup, args)
    }

    /// Stops a running backup; its `.part` files stay for a resume. Returns when it is gone.
    /// Nothing else is stopped by it.
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
        let args = self.import_args(backup_dir, true, replace, allow_empty)?;
        self.inner.start(JobKind::ImportDryRun, args)
    }

    /// `cloud import` of the backup in `backup_dir`, with the server stopped.
    pub fn import(
        &self,
        backup_dir: &Path,
        replace: bool,
        allow_empty: bool,
    ) -> Result<(), String> {
        let args = self.import_args(backup_dir, false, replace, allow_empty)?;
        self.inner.start(JobKind::Import, args)
    }

    /// `cloud import-app-data --dry-run` of `snbak`; `None` is `--empty`.
    pub fn app_data_dry_run(&self, snbak: Option<&Path>, replace: bool) -> Result<(), String> {
        let args = self.app_data_args(snbak, true, replace)?;
        self.inner.start(JobKind::AppDataDryRun, args)
    }

    /// `cloud import-app-data` of `snbak` (`None` is `--empty`), with the server stopped.
    pub fn app_data(&self, snbak: Option<&Path>, replace: bool) -> Result<(), String> {
        let args = self.app_data_args(snbak, false, replace)?;
        self.inner.start(JobKind::AppData, args)
    }

    /// A job runs.
    pub fn busy(&self) -> bool {
        lock(&self.inner.run).kind.is_some()
    }

    /// The running job.
    pub fn state(&self) -> Option<JobKind> {
        lock(&self.inner.run).kind
    }

    /// The guard of the commands that touch the server or the login: refused while a job runs.
    pub fn not_busy(&self) -> Result<(), String> {
        if self.busy() {
            Err(WAIT_FOR_IMPORT.to_owned())
        } else {
            Ok(())
        }
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
}

impl Inner {
    /// Starts `kind` with `args` and `--json --exit-with-parent <pid>`. An import first stops
    /// the app's server and pipeline; it is refused while either runs outside the app.
    fn start(self: &Arc<Self>, kind: JobKind, mut args: Vec<String>) -> Result<(), String> {
        let holds = kind.holds_server();
        if holds && self.host.outside() {
            return Err(OUTSIDE_LINE.to_owned());
        }
        {
            let mut run = lock(&self.run);
            if run.quitting {
                return Err(QUITTING.to_owned());
            }
            if run.kind.is_some() {
                return Err(ANOTHER_RUNS.to_owned());
            }
            // Taken before the stop, which can take the children's grace: no second job and
            // no command of the guard gets in meanwhile.
            run.kind = Some(kind);
            run.stop_requested = false;
        }
        if holds {
            self.host.stop_all();
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
                run.kind = None;
                drop(run);
                if holds {
                    self.host.start();
                    self.host.poll_now();
                }
                return Err(format!("The notedown program could not start: {e}"));
            }
        };
        run.pgid = Some(child.id());
        drop(run);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let inner = self.clone();
        std::thread::spawn(move || inner.follow(kind, child, stdout, stderr));
        Ok(())
    }

    fn quitting(&self) -> bool {
        lock(&self.run).quitting
    }

    /// Reads the job's lines until it ends, starts the server again after an import and
    /// reports how the job ended.
    fn follow(
        self: Arc<Self>,
        kind: JobKind,
        mut child: Child,
        stdout: Option<ChildStdout>,
        stderr: Option<ChildStderr>,
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
        let status = child.wait();
        // Reaped: a stop must not signal the group from now on.
        {
            let mut run = lock(&self.run);
            run.pgid = None;
            self.reaped.notify_all();
        }
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
        let event = match (status, lock(&last).take()) {
            (Ok(status), Some(mut json)) => {
                let ok = status.success();
                if !ok && let Some(object) = json.as_object_mut() {
                    let error = process::error_text(&lock(&errors), status);
                    object.insert("error".to_owned(), serde_json::Value::String(error));
                }
                JobEvent::Done(JobResult { kind, ok, json })
            }
            (Ok(status), None) => JobEvent::Failed {
                kind,
                text: process::error_text(&lock(&errors), status),
            },
            (Err(e), _) => JobEvent::Failed {
                kind,
                text: format!("the step could not be followed: {e}"),
            },
        };
        // The server starts again, also after a failure; never when the app quits.
        if kind.holds_server() && !self.quitting() {
            self.host.start();
            self.host.poll_now();
            if kind == JobKind::AppData && matches!(&event, JobEvent::Done(r) if r.ok) {
                self.host.imported(&|| self.quitting());
            }
        }
        lock(&self.run).kind = None;
        if !stopped {
            (self.sink)(&event);
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

    /// SIGTERM to the job's group, SIGKILL after the grace; returns when it is reaped. With
    /// `only`, a job of another kind runs on.
    fn stop(&self, only: Option<JobKind>) {
        let mut run = lock(&self.run);
        let Some(pgid) = run.pgid else {
            return;
        };
        if only.is_some() && run.kind != only {
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

/// The app's own children, the poller, the config and the Keychain.
pub struct AppHost {
    pub config: ConfigState,
    pub processes: Arc<Processes>,
    pub poller: Poller,
    pub store: Arc<dyn TokenStore>,
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

    fn imported(&self, quitting: &dyn Fn() -> bool) {
        if let Err(why) = self.config.imports_done() {
            eprintln!("notedown: the backup choices were not cleared: {why}");
        }
        // The server that just started again needs a moment before it answers.
        let until = Instant::now() + MARK_WAIT;
        loop {
            if quitting() {
                return;
            }
            let config = self.config.current();
            let Some(base) = poller::rest_base(&config, &self.processes.serve_info()) else {
                return;
            };
            match mark_once(&self.config, &base, &*self.store) {
                Ok(_) => return,
                Err(why) if Instant::now() >= until => {
                    eprintln!("notedown: the import mark was not taken: {why}");
                    return;
                }
                Err(_) => std::thread::sleep(MARK_EVERY),
            }
        }
    }
}

#[cfg(test)]
mod tests;
