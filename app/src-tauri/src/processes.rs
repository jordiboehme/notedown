//! Both children together: the start that first stops what an earlier run of the app left
//! behind and then looks for another Notedown, the probe again of what is `Outside`, and the
//! stop of both at once.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::Serialize;

use crate::config::{AppConfig, Mode};
use crate::pidfile;
use crate::probe::{self, ServerProbe};
use crate::process::{self, ChildKind, Launch, SpawnFn};
use crate::serve_info::{self, ServeInfo};
use crate::supervisor::{ChildState, Clock, ProcessSupervisor};

/// How often the app looks again whether an `Outside` process is gone.
pub const REPROBE_EVERY: Duration = Duration::from_secs(30);

/// What [`Processes::new`] needs.
pub struct Options {
    pub spawn: SpawnFn,
    pub clock: Clock,
    /// The sidecar: a child of an earlier run is ours when it runs this program.
    pub sidecar: PathBuf,
    /// Where the pid files are, `<app data dir>/run`.
    pub run_dir: PathBuf,
    /// The app's pid, for `--exit-with-parent`.
    pub parent: u32,
    pub server_grace: Duration,
    pub pipeline_grace: Duration,
}

/// The states of both children, for the webview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProcessStates {
    pub server: ChildState,
    pub pipeline: ChildState,
}

/// The server and the pipeline supervisors.
pub struct Processes {
    server: ProcessSupervisor,
    pipeline: ProcessSupervisor,
    sidecar: PathBuf,
    run_dir: PathBuf,
    parent: u32,
    server_grace: Duration,
    pipeline_grace: Duration,
    serve_info: Arc<Mutex<ServeInfo>>,
    /// Told when a start line changed the serve info.
    serve_listeners: Arc<Mutex<Vec<ServeListener>>>,
    /// Serializes start, probe and stop, so a probe never undoes a stop.
    ops: Mutex<()>,
    /// What the pipeline was last told to do; `None` before the first start and after a stop.
    pipeline_launch: Mutex<Option<Launch>>,
    quitting: AtomicBool,
}

type ServeListener = Box<dyn Fn() + Send + Sync>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Processes {
    pub fn new(options: Options) -> Self {
        let server = ProcessSupervisor::new(
            ChildKind::Server,
            options.spawn.clone(),
            options.clock.clone(),
            options.server_grace,
        );
        let pipeline = ProcessSupervisor::new(
            ChildKind::Pipeline,
            options.spawn,
            options.clock,
            options.pipeline_grace,
        );
        for sup in [&server, &pipeline] {
            sup.keep_pid_file(
                pidfile::path(&options.run_dir, sup.kind()),
                options.sidecar.clone(),
            );
        }
        let serve_info = Arc::new(Mutex::new(ServeInfo::default()));
        let serve_listeners: Arc<Mutex<Vec<ServeListener>>> = Arc::default();
        let (info, listeners) = (serve_info.clone(), serve_listeners.clone());
        server.on_line(move |line| {
            let changed = {
                let mut info = lock(&info);
                let before = info.clone();
                serve_info::parse_line(&mut info, line);
                *info != before
            };
            if changed {
                for listener in lock(&listeners).iter() {
                    listener();
                }
            }
        });
        Self {
            server,
            pipeline,
            sidecar: options.sidecar,
            run_dir: options.run_dir,
            parent: options.parent,
            server_grace: options.server_grace,
            pipeline_grace: options.pipeline_grace,
            serve_info,
            serve_listeners,
            ops: Mutex::new(()),
            pipeline_launch: Mutex::new(None),
            quitting: AtomicBool::new(false),
        }
    }

    pub fn server(&self) -> &ProcessSupervisor {
        &self.server
    }

    pub fn pipeline(&self) -> &ProcessSupervisor {
        &self.pipeline
    }

    pub fn states(&self) -> ProcessStates {
        ProcessStates {
            server: self.server.state(),
            pipeline: self.pipeline.state(),
        }
    }

    /// Runs `cb` whenever a start line of the server changed what [`serve_info`](Self::serve_info)
    /// knows (a new address). It runs on the server's output thread, so it must not block.
    pub fn on_serve_info(&self, cb: impl Fn() + Send + Sync + 'static) {
        lock(&self.serve_listeners).push(Box::new(cb));
    }

    /// The start lines of the running server; empty while it does not run.
    pub fn serve_info(&self) -> ServeInfo {
        if matches!(self.server.state(), ChildState::Running { .. }) {
            lock(&self.serve_info).clone()
        } else {
            ServeInfo::default()
        }
    }

    /// Starts what the config asks for, afresh:
    /// 1. stops both children, so the probes do not find the app's own;
    /// 2. stops what an earlier run of the app left behind (its pid files);
    /// 3. per child, probes for another Notedown: found, the child is `Outside`; else it starts.
    pub fn start_all(&self, config: &AppConfig) {
        self.start_with(|| config.clone());
    }

    /// [`start_all`](Self::start_all) with the config read once the start holds the lock, so
    /// an older snapshot cannot win against a newer start or a stop. The app passes the live
    /// config.
    pub fn start_with(&self, read: impl FnOnce() -> AppConfig) {
        let _ops = lock(&self.ops);
        if self.quitting.load(Ordering::SeqCst) {
            return;
        }
        let config = &read();
        self.stop_both();
        if config.mode == Mode::Local {
            // On a fresh Mac the folder is not there yet; the pipeline would fail at its first
            // start without it. A failure here is only logged: the child then says what is wrong.
            if let Err(e) = std::fs::create_dir_all(&config.data_dir) {
                eprintln!(
                    "notedown: the data folder {} could not be made: {e}",
                    config.data_dir.display()
                );
            }
        }
        std::thread::scope(|s| {
            for sup in [&self.server, &self.pipeline] {
                s.spawn(move || self.reclaim(sup));
            }
        });
        let plan = process::plan(config, self.parent);
        *lock(&self.pipeline_launch) = Some(plan.pipeline.clone());
        self.launch(&self.server, &plan.server, config);
        self.launch(&self.pipeline, &plan.pipeline, config);
    }

    /// Probes again what is `Outside` and starts it when the other process is gone. Skipped
    /// while a start or a stop runs.
    pub fn reprobe(&self, config: &AppConfig) {
        self.reprobe_with(|| config.clone());
    }

    /// [`reprobe`](Self::reprobe) with the config read inside the lock.
    pub fn reprobe_with(&self, read: impl FnOnce() -> AppConfig) {
        let Ok(_ops) = self.ops.try_lock() else {
            return;
        };
        if self.quitting.load(Ordering::SeqCst) {
            return;
        }
        let config = &read();
        let plan = process::plan(config, self.parent);
        for (sup, launch) in [
            (&self.server, &plan.server),
            (&self.pipeline, &plan.pipeline),
        ] {
            if matches!(sup.state(), ChildState::Outside { .. }) {
                if sup.kind() == ChildKind::Pipeline {
                    *lock(&self.pipeline_launch) = Some(launch.clone());
                }
                self.launch(sup, launch, config);
            }
        }
    }

    /// Starts the pipeline again when what it is to run changed (another Markdown folder, a log
    /// in or a log out), with the config read inside the lock. The server runs on. Nothing
    /// starts after a stop, before the first start or after Quit.
    pub fn relaunch_pipeline_with(&self, read: impl FnOnce() -> AppConfig) {
        let _ops = lock(&self.ops);
        if self.quitting.load(Ordering::SeqCst) {
            return;
        }
        let config = &read();
        let launch = process::plan(config, self.parent).pipeline;
        {
            let mut last = lock(&self.pipeline_launch);
            match &*last {
                Some(before) if *before != launch => *last = Some(launch.clone()),
                _ => return,
            }
        }
        self.launch(&self.pipeline, &launch, config);
    }

    /// Stops both children at once. They stay stopped until the next start.
    pub fn stop_all(&self) {
        let _ops = lock(&self.ops);
        *lock(&self.pipeline_launch) = None;
        self.stop_both();
    }

    /// Stops both children for good: nothing starts again after it. Quit, Cmd+Q and the end of
    /// the app all come here.
    pub fn quit(&self) {
        self.quitting.store(true, Ordering::SeqCst);
        self.stop_all();
    }

    /// Both stops at once: each can take its grace.
    fn stop_both(&self) {
        std::thread::scope(|s| {
            s.spawn(|| self.server.stop());
            self.pipeline.stop();
        });
    }

    fn reclaim(&self, sup: &ProcessSupervisor) {
        let kind = sup.kind();
        let path = pidfile::path(&self.run_dir, kind);
        // The orphan's own grace: the pipeline ends between two pages.
        pidfile::reclaim(&path, &self.sidecar, self.grace(kind));
    }

    fn grace(&self, kind: ChildKind) -> Duration {
        match kind {
            ChildKind::Server => self.server_grace,
            ChildKind::Pipeline => self.pipeline_grace,
        }
    }

    /// Starts the child unless another Notedown does its job.
    fn launch(&self, sup: &ProcessSupervisor, launch: &Launch, config: &AppConfig) {
        let Launch::Start(args) = launch else {
            sup.launch(launch);
            return;
        };
        let kind = sup.kind();
        if self.found_outside(kind, config) {
            sup.outside(kind.name());
        } else {
            sup.start(args.clone());
        }
    }

    /// Whether another Notedown does the job of `kind`: a server answering on the port, or a
    /// pipeline holding the writer lock. Another program on the port is not Notedown; `serve`
    /// then fails to bind and says so in its backoff.
    fn found_outside(&self, kind: ChildKind, config: &AppConfig) -> bool {
        match kind {
            ChildKind::Server => {
                let addr = serve_info::server_toml_addr(&config.data_dir);
                probe::server_at(addr) == ServerProbe::Notedown
            }
            ChildKind::Pipeline => probe::pipeline_lock(&config.data_dir),
        }
    }
}

#[cfg(test)]
mod tests;
