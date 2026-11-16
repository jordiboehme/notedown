//! The OCR model: its state (`model path`, `model verify`), the download (`model pull --json`)
//! with Pause, and Remove.
//!
//! Only one pull runs at a time. It runs in a process group of its own, so Pause and Quit stop
//! it with everything it started. After its `ready` line the model is checked again with
//! `model verify`.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Output, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;

use crate::process::{self, SpawnFn};

pub const MODEL_PROGRESS_EVENT: &str = "model-progress";
pub const MODEL_READY_EVENT: &str = "model-ready";
pub const MODEL_FAILED_EVENT: &str = "model-failed";

/// What a second Download or a Remove during a download hears.
pub const PULL_RUNS: &str = "A download runs already.";

/// What Download or a second Remove hears while the model is removed.
pub const REMOVE_RUNS: &str = "The model is being removed.";

const QUITTING: &str = "Notedown is quitting.";

/// One progress line of `model pull --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Progress {
    pub file: String,
    pub done: u64,
    pub total: u64,
}

/// The model as the OCR page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelState {
    /// `model verify` says every file is fine.
    pub installed: bool,
    /// The bytes of the model files on disk, a part of a download included.
    pub size_bytes: u64,
    /// The model file.
    pub path: Option<String>,
    /// A download runs (or its result is being checked).
    pub pulling: bool,
    /// The last progress of the running download.
    pub progress: Option<Progress>,
    /// The state could not be read.
    pub problem: Option<String>,
}

/// What a pull tells the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelEvent {
    Progress(Progress),
    /// The download ended and `model verify` was asked again.
    Ready(ModelState),
    /// The download failed; the text is its error.
    Failed {
        text: String,
    },
}

/// Gets every event. It must not block: the app only sends it into a channel.
pub type Sink = Arc<dyn Fn(&ModelEvent) + Send + Sync>;

/// How long `model path` and `model verify` may take. Verify reads 3.4 GB through SHA-256.
const CHECK_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a stop waits for the pull to be reaped after SIGKILL.
const GONE_WAIT: Duration = Duration::from_secs(5);
/// How long the end of a pull waits for its last stdout and stderr lines.
const DRAIN: Duration = Duration::from_secs(1);
/// The stderr lines a pull keeps for its error text.
const STDERR_LINES: usize = 50;

/// The model download and the model files. Cheap to clone; every clone is the same control.
#[derive(Clone)]
pub struct ModelControl {
    inner: Arc<Inner>,
}

struct Inner {
    spawn: SpawnFn,
    grace: Duration,
    sink: Sink,
    run: Mutex<Run>,
    /// Signals the end of a pull's process.
    reaped: Condvar,
    /// The last `model verify` answer, for files that have not changed since.
    verified: Mutex<Option<(Vec<FileMark>, bool)>>,
}

#[derive(Default)]
struct Run {
    /// From the spawn of a pull to the end of its check.
    busy: bool,
    /// The process group of the running pull, until it is reaped.
    pgid: Option<u32>,
    /// Pause or Quit asked the pull to stop: its end is no failure.
    stop_requested: bool,
    progress: Option<Progress>,
    /// After Quit nothing starts.
    quitting: bool,
    /// Remove runs: no download starts meanwhile.
    removing: bool,
    /// The process groups of the running `model path` and `model verify` calls, until each is
    /// reaped. Quit ends them.
    checks: Vec<u32>,
}

/// What a model file looks like on disk: a change means it must be verified again.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileMark {
    path: PathBuf,
    len: Option<u64>,
    modified: Option<SystemTime>,
}

/// The files `model path` names.
struct Paths {
    model: PathBuf,
    mmproj: PathBuf,
}

impl Paths {
    /// The model files and the parts of a download.
    fn all(&self) -> [PathBuf; 4] {
        [
            self.model.clone(),
            self.mmproj.clone(),
            part(&self.model),
            part(&self.mmproj),
        ]
    }
}

fn part(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".part");
    PathBuf::from(name)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

impl ModelControl {
    /// `grace` is the wait between SIGTERM and SIGKILL when a pull is stopped.
    pub fn new(spawn: SpawnFn, grace: Duration, sink: Sink) -> Self {
        Self {
            inner: Arc::new(Inner {
                spawn,
                grace,
                sink,
                run: Mutex::new(Run::default()),
                reaped: Condvar::new(),
                verified: Mutex::new(None),
            }),
        }
    }

    /// The model as it is now. Blocking: it runs `model path` and, when the files changed,
    /// `model verify`.
    pub fn state(&self) -> ModelState {
        self.inner.state()
    }

    /// Starts the download. Refused while one runs and after Quit.
    pub fn pull(&self) -> Result<(), String> {
        let mut run = lock(&self.inner.run);
        if run.quitting {
            return Err(QUITTING.to_owned());
        }
        if run.busy {
            return Err(PULL_RUNS.to_owned());
        }
        if run.removing {
            return Err(REMOVE_RUNS.to_owned());
        }
        let mut command = (self.inner.spawn)(&args(&["model", "pull", "--json"]));
        command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| format!("notedown could not start: {e}"))?;
        run.busy = true;
        run.pgid = Some(child.id());
        run.stop_requested = false;
        run.progress = None;
        drop(run);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let inner = self.inner.clone();
        std::thread::spawn(move || inner.follow(child, stdout, stderr));
        Ok(())
    }

    /// Stops a running download; Download continues it later. Returns when it is gone.
    pub fn pause(&self) {
        self.inner.stop();
    }

    /// Stops a running download and the running checks for good: nothing starts after it.
    pub fn quit(&self) {
        {
            let mut run = lock(&self.inner.run);
            run.quitting = true;
            // A check has nothing to save. Each group is still unreaped while it is listed.
            for pgid in &run.checks {
                let _ = process::signal_group(*pgid, libc::SIGKILL);
            }
        }
        self.inner.stop();
    }

    /// Deletes the model files and the parts of a download. Refused while a download runs;
    /// no download starts until it is over.
    pub fn remove(&self) -> Result<ModelState, String> {
        {
            let mut run = lock(&self.inner.run);
            if run.busy {
                return Err(PULL_RUNS.to_owned());
            }
            if run.removing {
                return Err(REMOVE_RUNS.to_owned());
            }
            run.removing = true;
        }
        let removed = self.delete_files();
        lock(&self.inner.run).removing = false;
        removed?;
        Ok(self.inner.state())
    }

    fn delete_files(&self) -> Result<(), String> {
        let paths = self.inner.paths()?;
        for path in paths.all() {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{} could not be deleted: {e}", path.display())),
            }
        }
        *lock(&self.inner.verified) = None;
        Ok(())
    }
}

impl Inner {
    fn state(&self) -> ModelState {
        let (pulling, progress) = {
            let run = lock(&self.run);
            (run.busy, run.progress.clone())
        };
        self.read_state(pulling, progress)
    }

    /// The state from the files; `pulling` skips `model verify` while a download writes them.
    fn read_state(&self, pulling: bool, progress: Option<Progress>) -> ModelState {
        let mut state = ModelState {
            installed: false,
            size_bytes: 0,
            path: None,
            pulling,
            progress,
            problem: None,
        };
        let paths = match self.paths() {
            Ok(paths) => paths,
            Err(why) => {
                state.problem = Some(why);
                return state;
            }
        };
        let marks: Vec<FileMark> = paths.all().iter().map(|p| mark(p)).collect();
        state.size_bytes = marks.iter().filter_map(|m| m.len).sum();
        state.path = Some(paths.model.display().to_string());
        // A running download changes the files all the time; verify only when it is over.
        if pulling {
            return state;
        }
        let known = lock(&self.verified)
            .as_ref()
            .filter(|(seen, _)| *seen == marks)
            .map(|(_, ok)| *ok);
        state.installed = match known {
            Some(ok) => ok,
            None => match self.check(&["model", "verify"]) {
                Ok(output) => {
                    let ok = output.status.success();
                    *lock(&self.verified) = Some((marks, ok));
                    ok
                }
                Err(why) => {
                    state.problem = Some(format!("The model could not be checked: {why}"));
                    false
                }
            },
        };
        state
    }

    /// Runs `model path` or `model verify` in a process group of its own that Quit can end.
    /// Refused after Quit; a call that Quit ended is an error, never an answer.
    fn check(&self, list: &[&str]) -> Result<Output, String> {
        let mut command = (self.spawn)(&args(list));
        command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = {
            let mut run = lock(&self.run);
            if run.quitting {
                return Err(QUITTING.to_owned());
            }
            let child = command
                .spawn()
                .map_err(|e| format!("notedown could not start: {e}"))?;
            run.checks.push(child.id());
            child
        };
        let pid = child.id();
        let (tx, rx) = mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(move || {
                let output = child.wait_with_output();
                // Reaped: no signal to this group from now on.
                lock(&self.run).checks.retain(|p| *p != pid);
                let _ = tx.send(output);
            });
            let output = match rx.recv_timeout(CHECK_TIMEOUT) {
                Ok(output) => output.map_err(|e| format!("notedown could not be read: {e}")),
                Err(_) => {
                    let run = lock(&self.run);
                    if run.checks.contains(&pid) {
                        let _ = process::signal_group(pid, libc::SIGKILL);
                    }
                    drop(run);
                    Err("it did not answer in time".to_owned())
                }
            };
            if lock(&self.run).quitting {
                return Err(QUITTING.to_owned());
            }
            output
        })
    }

    /// The model files, from `model path`.
    fn paths(&self) -> Result<Paths, String> {
        let output = self
            .check(&["model", "path"])
            .map_err(|why| format!("The model folder is not known: {why}"))?;
        if !output.status.success() {
            return Err(format!(
                "The model folder is not known: {}",
                process::error_text(&output.stderr, output.status)
            ));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let line = |key: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(key))
                .map(|v| PathBuf::from(v.trim()))
        };
        match (line("model: "), line("mmproj: ")) {
            (Some(model), Some(mmproj)) => Ok(Paths { model, mmproj }),
            _ => Err("The model folder is not known: notedown did not name it".to_owned()),
        }
    }

    /// Reads the pull's lines until it ends, then reports how it ended.
    fn follow(
        self: Arc<Self>,
        mut child: Child,
        stdout: Option<ChildStdout>,
        stderr: Option<ChildStderr>,
    ) {
        let ready = Arc::new(Mutex::new(None::<String>));
        let errors = Arc::new(Mutex::new(Vec::<u8>::new()));
        let (done_tx, done_rx) = mpsc::channel::<()>();
        if let Some(stdout) = stdout {
            let inner = self.clone();
            let ready = ready.clone();
            let done = done_tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    inner.read_line(&line, &ready);
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
        let stopped = {
            let mut run = lock(&self.run);
            run.progress = None;
            run.stop_requested
        };
        let event = if stopped {
            None
        } else {
            match status {
                Ok(status) if status.success() && lock(&ready).is_some() => {
                    // Checked again, so the page shows what is really on disk. The pull
                    // counts as running until the check is over, so no second one starts.
                    *lock(&self.verified) = None;
                    let state = self.read_state(false, None);
                    Some(if state.installed {
                        ModelEvent::Ready(state)
                    } else {
                        ModelEvent::Failed {
                            text: "The download ended, but the model check failed.".to_owned(),
                        }
                    })
                }
                Ok(status) => Some(ModelEvent::Failed {
                    text: process::error_text(&lock(&errors), status),
                }),
                Err(e) => Some(ModelEvent::Failed {
                    text: format!("the download could not be followed: {e}"),
                }),
            }
        };
        lock(&self.run).busy = false;
        if let Some(event) = event {
            (self.sink)(&event);
        }
    }

    fn read_line(&self, line: &str, ready: &Mutex<Option<String>>) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        if value.get("ready").is_none() {
            let Ok(progress) = serde_json::from_value::<Progress>(value) else {
                return;
            };
            {
                let mut run = lock(&self.run);
                if run.stop_requested {
                    // Paused: what it says while it ends is no progress.
                    return;
                }
                run.progress = Some(progress.clone());
            }
            (self.sink)(&ModelEvent::Progress(progress));
            return;
        }
        if let Some(path) = value.get("ready").and_then(|v| v.as_str()) {
            *lock(ready) = Some(path.to_owned());
        }
    }

    /// SIGTERM to the pull's group, SIGKILL after the grace; returns when it is reaped.
    fn stop(&self) {
        let mut run = lock(&self.run);
        let Some(pgid) = run.pgid else {
            return;
        };
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

fn mark(path: &Path) -> FileMark {
    let meta = fs::metadata(path).ok();
    FileMark {
        path: path.to_owned(),
        len: meta.as_ref().map(|m| m.len()),
        modified: meta.and_then(|m| m.modified().ok()),
    }
}

#[cfg(test)]
mod tests;
