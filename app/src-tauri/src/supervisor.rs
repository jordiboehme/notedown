//! Runs one child (`serve` or `pipeline`), restarts it after a crash with a backoff, and stops it
//! with its whole process group.
//!
//! Each child gets its own process group, so a stop reaches what the child started too. A
//! supervising thread spawns the child and reaps it with `wait()`; the exit is seen from there,
//! never from the end of a pipe, which a grandchild may hold open. Two reader threads only fill
//! the log ring.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::pidfile::{self, PidFile};
use crate::process::{self, ChildKind, Launch, SpawnFn};

/// Milliseconds since the Unix epoch. Injected, so tests move time by hand.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The wall clock.
pub fn system_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    })
}

/// The part of the pipeline's refusal when another pipeline holds the writer lock
/// (`notedown_pipeline::Error::WriterBusy`, printed as `error: <text>`).
pub const WRITER_BUSY: &str = "another notedown pipeline runs for this data folder";

/// How many lines of stdout and stderr the log keeps.
pub const LOG_LINES: usize = 500;
/// A run this long resets the backoff.
const RESET_AFTER_MS: u64 = 5 * 60 * 1000;
/// How long the reaper waits for the last stderr lines after the exit. Bounded, as a
/// grandchild can hold the pipe.
const DRAIN: Duration = Duration::from_millis(250);
/// How often a backoff looks at the clock.
const TICK: Duration = Duration::from_millis(20);
/// How long a stop waits for the group to be gone after SIGKILL.
const GONE_WAIT: Duration = Duration::from_secs(5);

/// The state of one supervised child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ChildState {
    /// Not running, and not to run.
    Stopped,
    /// Remote mode without `pipeline.toml`: the pipeline is not set up yet.
    NotConnected,
    /// About to spawn.
    Starting,
    Running {
        pid: u32,
        since_ms: u64,
    },
    /// SIGTERM is sent; SIGKILL follows after the grace.
    Stopping,
    /// The child ended without a stop and starts again at `until_ms`. `last_error` is its last
    /// stderr line, or how it ended when it said nothing.
    Backoff {
        until_ms: u64,
        last_error: String,
    },
    /// Another Notedown process does this job; the app does not start a second one.
    Outside {
        what: String,
    },
}

/// The wait before restart number `attempt + 1`: 1, 2, 4, 8, 16, 32 s, then 60 s.
pub fn backoff_ms(attempt: u32) -> u64 {
    const SECS: [u64; 6] = [1, 2, 4, 8, 16, 32];
    let secs = usize::try_from(attempt)
        .ok()
        .and_then(|a| SECS.get(a).copied())
        .unwrap_or(60);
    secs * 1000
}

type Listener = Box<dyn Fn(&ChildState) + Send + Sync>;
type LineListener = Box<dyn Fn(&str) + Send + Sync>;

/// Runs one child. Dropping it stops the child.
pub struct ProcessSupervisor {
    inner: Arc<Inner>,
}

struct Inner {
    kind: ChildKind,
    spawn: SpawnFn,
    clock: Clock,
    grace: Duration,
    /// Serializes start and stop.
    control: Mutex<()>,
    shared: Mutex<Shared>,
    /// A copy of `Shared::state` for [`ProcessSupervisor::state`]. Nobody waits for anything
    /// while holding it, so a listener can read it while another thread holds `shared` and
    /// waits for the listeners.
    current: Mutex<ChildState>,
    changed: Condvar,
    listeners: Mutex<Vec<Listener>>,
    /// Called with every stdout line, on the reader thread.
    line_listeners: Arc<Mutex<Vec<LineListener>>>,
    log: Arc<Mutex<VecDeque<String>>>,
    /// The pid file and the program it names, while a child runs.
    pid_file: Mutex<Option<(PathBuf, PathBuf)>>,
}

struct Shared {
    state: ChildState,
    /// Bumped by every start and stop. A thread of an older generation changes nothing.
    generation: u64,
    /// The process group of the current child (its pid), until it is cleaned up.
    pgid: Option<u32>,
    /// The current child has not been reaped yet.
    alive: bool,
    worker: Option<JoinHandle<()>>,
}

/// How one run ended.
struct Ended {
    ran_ms: u64,
    last_error: String,
    busy: bool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ProcessSupervisor {
    /// `grace` is the wait between SIGTERM and SIGKILL ([`ChildKind::grace`] in the app).
    pub fn new(kind: ChildKind, spawn: SpawnFn, clock: Clock, grace: Duration) -> Self {
        let shared = Shared {
            state: ChildState::Stopped,
            generation: 0,
            pgid: None,
            alive: false,
            worker: None,
        };
        Self {
            inner: Arc::new(Inner {
                kind,
                spawn,
                clock,
                grace,
                control: Mutex::new(()),
                shared: Mutex::new(shared),
                current: Mutex::new(ChildState::Stopped),
                changed: Condvar::new(),
                listeners: Mutex::new(Vec::new()),
                line_listeners: Arc::new(Mutex::new(Vec::new())),
                log: Arc::new(Mutex::new(VecDeque::new())),
                pid_file: Mutex::new(None),
            }),
        }
    }

    pub fn kind(&self) -> ChildKind {
        self.inner.kind
    }

    /// Starts the child with `args` (a running one is stopped first) and keeps it running
    /// until [`stop`](Self::stop).
    pub fn start(&self, args: Vec<String>) {
        let _control = lock(&self.inner.control);
        self.inner.stop();
        let mut s = lock(&self.inner.shared);
        s.generation += 1;
        let generation = s.generation;
        let inner = self.inner.clone();
        s.worker = Some(std::thread::spawn(move || inner.run(generation, args)));
        self.inner.set(s, ChildState::Starting);
    }

    /// Stops the child: SIGTERM to its group, SIGKILL after the grace. Returns when the group
    /// is gone. Nothing restarts after it.
    pub fn stop(&self) {
        let _control = lock(&self.inner.control);
        self.inner.stop();
    }

    /// Stops the child and shows that another Notedown process does its job (`what`).
    pub fn outside(&self, what: &str) {
        let _control = lock(&self.inner.control);
        self.inner.stop();
        let mut s = lock(&self.inner.shared);
        s.generation += 1;
        self.inner.set(
            s,
            ChildState::Outside {
                what: what.to_string(),
            },
        );
    }

    /// Keeps a pid file at `path` while a child runs; `exe` is the program it names.
    pub fn keep_pid_file(&self, path: PathBuf, exe: PathBuf) {
        *lock(&self.inner.pid_file) = Some((path, exe));
    }

    /// Calls `cb` with every stdout line of every run, in order. The same rules as for
    /// [`on_change`](Self::on_change) hold, and `cb` runs on a reader thread, so it must be
    /// quick.
    pub fn on_line(&self, cb: impl Fn(&str) + Send + Sync + 'static) {
        lock(&self.inner.line_listeners).push(Box::new(cb));
    }

    /// Stops the child and shows that the pipeline is not set up yet.
    pub fn not_connected(&self) {
        let _control = lock(&self.inner.control);
        self.inner.stop();
        let mut s = lock(&self.inner.shared);
        s.generation += 1;
        self.inner.set(s, ChildState::NotConnected);
    }

    /// Does what [`process::plan`] decided for this child.
    pub fn launch(&self, launch: &Launch) {
        match launch {
            Launch::Off => self.stop(),
            Launch::NotConnected => self.not_connected(),
            Launch::Start(args) => self.start(args.clone()),
        }
    }

    pub fn state(&self) -> ChildState {
        lock(&self.inner.current).clone()
    }

    /// The last [`LOG_LINES`] lines of stdout and stderr, over restarts.
    pub fn log(&self) -> Vec<String> {
        lock(&self.inner.log).iter().cloned().collect()
    }

    /// Calls `cb` with every new state, in order. `cb` may call [`state`](Self::state) and
    /// [`log`](Self::log), but not `on_change`, `start` or `stop`.
    pub fn on_change(&self, cb: impl Fn(&ChildState) + Send + Sync + 'static) {
        lock(&self.inner.listeners).push(Box::new(cb));
    }
}

impl Drop for ProcessSupervisor {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Inner {
    /// Sets the state and tells the listeners. The listener lock is taken before the state
    /// lock is let go, so listeners see the changes in the order they were made.
    fn set(&self, mut s: MutexGuard<'_, Shared>, state: ChildState) {
        if s.state == state {
            return;
        }
        s.state = state.clone();
        *lock(&self.current) = state.clone();
        let listeners = lock(&self.listeners);
        drop(s);
        self.changed.notify_all();
        for cb in listeners.iter() {
            cb(&state);
        }
    }

    /// Sets the state only while `generation` is current.
    fn set_if(&self, generation: u64, state: ChildState) -> bool {
        let s = lock(&self.shared);
        if s.generation != generation {
            return false;
        }
        self.set(s, state);
        true
    }

    /// Ends the current generation; the caller holds `control`.
    fn stop(&self) {
        let mut s = lock(&self.shared);
        s.generation += 1;
        let worker = s.worker.take();
        let pgid = s.pgid;
        let term = pgid.filter(|_| s.alive);
        if term.is_some() {
            self.set(s, ChildState::Stopping);
        } else {
            drop(s);
            self.changed.notify_all();
        }

        if let Some(pgid) = term {
            let _ = process::signal_group(pgid, libc::SIGTERM);
            let until = Instant::now() + self.grace;
            let mut s = lock(&self.shared);
            while s.alive {
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                s = self
                    .changed
                    .wait_timeout(s, left)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        }
        // Also what ignored SIGTERM or outlived the leader.
        if let Some(pgid) = pgid {
            let _ = process::signal_group(pgid, libc::SIGKILL);
        }
        if let Some(worker) = worker {
            let _ = worker.join();
        }
        if let Some(pgid) = pgid {
            let until = Instant::now() + GONE_WAIT;
            while process::group_alive(pgid) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        // Only the file of this supervisor's own child: a pid file of an earlier run of the
        // app stays for the start to find.
        if pgid.is_some() {
            self.remove_pid_file();
        }
        let mut s = lock(&self.shared);
        s.pgid = None;
        s.alive = false;
        self.set(s, ChildState::Stopped);
    }

    /// Writes the pid file of a child that just started. A failure only goes to the log: the
    /// child runs all the same, only a crash of the app would leave it unknown.
    fn write_pid_file(&self, pid: u32, started_ms: u64) {
        let Some((path, exe)) = lock(&self.pid_file).clone() else {
            return;
        };
        let pf = PidFile {
            pid,
            pgid: pid,
            started_ms,
            exe,
        };
        if let Err(e) = pidfile::write(&path, &pf) {
            push_line(&self.log, format!("the pid file was not written: {e}"));
        }
    }

    fn remove_pid_file(&self) {
        if let Some((path, _)) = lock(&self.pid_file).as_ref() {
            pidfile::remove(path);
        }
    }

    /// The supervising thread of one generation: spawn, reap, back off, again.
    fn run(&self, generation: u64, args: Vec<String>) {
        let mut attempt: u32 = 0;
        loop {
            let Some(ended) = self.run_once(generation, &args) else {
                return;
            };
            if self.kind == ChildKind::Pipeline && ended.busy {
                self.set_if(
                    generation,
                    ChildState::Outside {
                        what: ChildKind::Pipeline.name().to_string(),
                    },
                );
                return;
            }
            if ended.ran_ms >= RESET_AFTER_MS {
                attempt = 0;
            }
            let until_ms = (self.clock)().saturating_add(backoff_ms(attempt));
            attempt = attempt.saturating_add(1);
            let backoff = ChildState::Backoff {
                until_ms,
                last_error: ended.last_error,
            };
            if !self.set_if(generation, backoff) {
                return;
            }
            let mut s = lock(&self.shared);
            loop {
                if s.generation != generation {
                    return;
                }
                if (self.clock)() >= until_ms {
                    break;
                }
                s = self
                    .changed
                    .wait_timeout(s, TICK)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
            self.set(s, ChildState::Starting);
        }
    }

    /// Spawns the child and waits for its exit. `None` when a stop ended this generation.
    fn run_once(&self, generation: u64, args: &[String]) -> Option<Ended> {
        let mut cmd = (self.spawn)(args);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);

        // Spawned under the lock: a stop sees either no child or the child's group.
        let mut s = lock(&self.shared);
        if s.generation != generation {
            return None;
        }
        let started = (self.clock)();
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                drop(s);
                return Some(Ended {
                    ran_ms: 0,
                    last_error: format!("could not start: {e}"),
                    busy: false,
                });
            }
        };
        let pid = child.id();
        s.pgid = Some(pid);
        s.alive = true;
        // Before the state says Running, so whoever sees the pid finds its pid file too.
        self.write_pid_file(pid, started);
        self.set(
            s,
            ChildState::Running {
                pid,
                since_ms: started,
            },
        );

        let last = Arc::new(Mutex::new(None::<String>));
        let (eof_tx, eof_rx) = mpsc::channel::<()>();
        if let Some(out) = child.stdout.take() {
            self.read_lines(out, None, None, Some(self.line_listeners.clone()));
        }
        if let Some(err) = child.stderr.take() {
            self.read_lines(err, Some(last.clone()), Some(eof_tx), None);
        }

        let status = child.wait();
        let mut s = lock(&self.shared);
        s.alive = false;
        let stopped = s.generation != generation;
        drop(s);
        self.changed.notify_all();
        if stopped {
            return None;
        }

        // What the child left in its group goes with it, so it holds no lock or port.
        let _ = process::signal_group(pid, libc::SIGKILL);
        self.remove_pid_file();
        let _ = eof_rx.recv_timeout(DRAIN);
        let mut s = lock(&self.shared);
        if s.generation == generation {
            s.pgid = None;
        }
        drop(s);

        let said = lock(&last).take();
        let busy = said.as_deref().is_some_and(|l| l.contains(WRITER_BUSY));
        let last_error = said.unwrap_or_else(|| describe(status));
        let ran_ms = (self.clock)().saturating_sub(started);
        Some(Ended {
            ran_ms,
            last_error,
            busy,
        })
    }

    /// A thread that puts each line into the log; for stderr it keeps the last non-blank line
    /// and drops `eof` at the end of the pipe; for stdout it hands each line to `lines`.
    fn read_lines(
        &self,
        pipe: impl Read + Send + 'static,
        last: Option<Arc<Mutex<Option<String>>>>,
        eof: Option<mpsc::Sender<()>>,
        lines: Option<Arc<Mutex<Vec<LineListener>>>>,
    ) {
        let log = self.log.clone();
        std::thread::spawn(move || {
            let _eof = eof;
            let mut reader = BufReader::new(pipe);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let line = String::from_utf8_lossy(&buf)
                    .trim_end_matches(['\n', '\r'])
                    .to_string();
                if let Some(last) = &last
                    && !line.trim().is_empty()
                {
                    *lock(last) = Some(line.clone());
                }
                if let Some(lines) = &lines {
                    for cb in lock(lines).iter() {
                        cb(&line);
                    }
                }
                push_line(&log, line);
            }
        });
    }
}

fn push_line(log: &Mutex<VecDeque<String>>, line: String) {
    let mut log = lock(log);
    if log.len() == LOG_LINES {
        log.pop_front();
    }
    log.push_back(line);
}

/// How a child ended, when it said nothing on stderr.
fn describe(status: std::io::Result<ExitStatus>) -> String {
    match status {
        Ok(status) => match (status.code(), status.signal()) {
            (Some(code), _) => format!("ended with exit code {code}"),
            (None, Some(signal)) => format!("ended by signal {signal}"),
            (None, None) => "ended".to_string(),
        },
        Err(e) => format!("could not wait for the child: {e}"),
    }
}

#[cfg(test)]
mod tests;
