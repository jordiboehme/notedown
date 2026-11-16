//! The two children the app runs, `notedown serve` and `notedown pipeline`: what each is, the
//! arguments each gets from the config, and how a child is spawned and signalled.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use crate::config::{self, AppConfig, Mode};

/// Which child a supervisor runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildKind {
    /// `notedown serve`, Local mode only.
    Server,
    /// `notedown pipeline`, both modes.
    Pipeline,
}

impl ChildKind {
    /// The name in the pid file, the status and the `Outside` state.
    pub fn name(self) -> &'static str {
        match self {
            ChildKind::Server => "server",
            ChildKind::Pipeline => "pipeline",
        }
    }

    /// The `notedown` subcommand.
    pub fn subcommand(self) -> &'static str {
        match self {
            ChildKind::Server => "serve",
            ChildKind::Pipeline => "pipeline",
        }
    }

    /// How long a stop waits after SIGTERM before SIGKILL. The pipeline ends between two pages,
    /// and one page of OCR can take a while.
    pub fn grace(self) -> Duration {
        match self {
            ChildKind::Server => Duration::from_secs(5),
            ChildKind::Pipeline => Duration::from_secs(15),
        }
    }
}

/// The arguments of a child: `<subcommand> --data <dir> --exit-with-parent <pid>`.
pub fn child_args(kind: ChildKind, data_dir: &Path, parent: u32) -> Vec<String> {
    vec![
        kind.subcommand().to_string(),
        "--data".to_string(),
        data_dir.display().to_string(),
        "--exit-with-parent".to_string(),
        parent.to_string(),
    ]
}

/// What a supervisor is to do with the config as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// The child does not run in this mode.
    Off,
    /// Remote mode without `pipeline.toml`: the pipeline is not set up yet.
    NotConnected,
    /// Start the child with these arguments.
    Start(Vec<String>),
}

/// The launch of both children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub server: Launch,
    pub pipeline: Launch,
}

/// Local mode runs both children; Remote mode runs the pipeline only, and only when
/// `<data>/pipeline.toml` exists; no mode runs nothing. `parent` is the app's pid.
pub fn plan(config: &AppConfig, parent: u32) -> Plan {
    let data = &config.data_dir;
    match config.mode {
        Mode::NotSet => Plan {
            server: Launch::Off,
            pipeline: Launch::Off,
        },
        Mode::Local => Plan {
            server: Launch::Start(child_args(ChildKind::Server, data, parent)),
            pipeline: Launch::Start(child_args(ChildKind::Pipeline, data, parent)),
        },
        Mode::Remote => Plan {
            server: Launch::Off,
            pipeline: if config::existing_setup(data).pipeline {
                Launch::Start(child_args(ChildKind::Pipeline, data, parent))
            } else {
                Launch::NotConnected
            },
        },
    }
}

/// Builds the command of a child from its arguments. The supervisor adds the process group and
/// the pipes, so a spawn function only picks the program.
pub type SpawnFn = Arc<dyn Fn(&[String]) -> Command + Send + Sync>;

/// The sidecar next to the app's executable, where Tauri puts `externalBin`.
pub fn sidecar_path() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| io::Error::other("the app's executable has no folder"))?;
    Ok(dir.join("notedown"))
}

/// A spawn function that runs `program` with the child's arguments.
pub fn spawn_program(program: PathBuf) -> SpawnFn {
    Arc::new(move |args: &[String]| {
        let mut cmd = Command::new(&program);
        cmd.args(args);
        cmd
    })
}

/// Runs a short `notedown` call to its end and returns what it printed. A call that takes
/// longer than `timeout` gets SIGKILL and is an error.
pub fn run_bounded(spawn: &SpawnFn, args: &[String], timeout: Duration) -> Result<Output, String> {
    let mut command: Command = spawn(args);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command
        .spawn()
        .map_err(|e| format!("the notedown program could not start: {e}"))?;
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(timeout) {
        Ok(output) => {
            output.map_err(|e| format!("the answer of the notedown program could not be read: {e}"))
        }
        Err(_) => {
            // The waiter has not reaped the child yet, so the pid is still this child's.
            if let Ok(pid) = i32::try_from(pid) {
                // SAFETY: kill only sends a signal; the pid belongs to our unreaped child.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            let _ = rx.recv();
            Err("it did not answer in time".to_owned())
        }
    }
}

/// What a failed `notedown` call says: its last `error: ` line without that prefix (not the
/// warnings before it), else all of its stderr lines, else how it ended.
pub fn error_text(stderr: &[u8], status: ExitStatus) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let text = match lines.iter().rev().find_map(|l| l.strip_prefix("error: ")) {
        Some(error) => error.to_owned(),
        None => lines.join(" "),
    };
    if !text.is_empty() {
        return text;
    }
    match status.code() {
        Some(code) => format!("it ended with exit code {code}"),
        None => "it ended by a signal".to_owned(),
    }
}

/// Sends `signal` to every process in the group `pgid`. A group that is gone is no error.
pub fn signal_group(pgid: u32, signal: i32) -> io::Result<()> {
    let pgid = i32::try_from(pgid).map_err(|_| io::Error::other("pgid out of range"))?;
    // SAFETY: killpg has no memory effects; it only sends a signal.
    if unsafe { libc::killpg(pgid, signal) } == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(e)
    }
}

/// Whether any process of the group `pgid` is still there.
pub fn group_alive(pgid: u32) -> bool {
    let Ok(pgid) = i32::try_from(pgid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the group exists and may be signalled.
    if unsafe { libc::killpg(pgid, 0) } == 0 {
        return true;
    }
    // EPERM: the group exists, but is not ours to signal.
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(mode: Mode, data_dir: &Path) -> AppConfig {
        AppConfig {
            mode,
            data_dir: data_dir.to_path_buf(),
            ..AppConfig::default()
        }
    }

    fn args(sub: &str, data: &Path, parent: u32) -> Vec<String> {
        vec![
            sub.to_string(),
            "--data".to_string(),
            data.display().to_string(),
            "--exit-with-parent".to_string(),
            parent.to_string(),
        ]
    }

    #[test]
    fn local_mode_plans_serve_and_the_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(&config(Mode::Local, dir.path()), 4242);
        assert_eq!(plan.server, Launch::Start(args("serve", dir.path(), 4242)));
        assert_eq!(
            plan.pipeline,
            Launch::Start(args("pipeline", dir.path(), 4242))
        );
    }

    #[test]
    fn remote_mode_runs_the_pipeline_only_with_pipeline_toml() {
        let dir = tempfile::tempdir().unwrap();
        let without = plan(&config(Mode::Remote, dir.path()), 7);
        assert_eq!(without.server, Launch::Off);
        assert_eq!(without.pipeline, Launch::NotConnected);

        std::fs::write(dir.path().join("pipeline.toml"), "").unwrap();
        let with = plan(&config(Mode::Remote, dir.path()), 7);
        assert_eq!(with.server, Launch::Off);
        assert_eq!(
            with.pipeline,
            Launch::Start(args("pipeline", dir.path(), 7))
        );
    }

    #[test]
    fn no_mode_runs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(&config(Mode::NotSet, dir.path()), 7);
        assert_eq!(plan.server, Launch::Off);
        assert_eq!(plan.pipeline, Launch::Off);
    }

    #[test]
    fn the_graces_are_the_decided_ones() {
        assert_eq!(ChildKind::Server.grace(), Duration::from_secs(5));
        assert_eq!(ChildKind::Pipeline.grace(), Duration::from_secs(15));
        assert_eq!(ChildKind::Server.name(), "server");
        assert_eq!(ChildKind::Pipeline.name(), "pipeline");
    }
}
