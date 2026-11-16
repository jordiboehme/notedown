//! The children are small shell scripts in a temp folder, the other Notedown server is a test
//! listener on loopback, and a child of an "earlier run" is a `/bin/sleep` that belongs to
//! launchd. The real `notedown` never runs here.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::*;
use crate::config::Mode;
use crate::pidfile::PidFile;
use crate::probe::test_server::{TestServer, free_port, notedown_401};
use crate::supervisor::system_clock;

const GRACE: Duration = Duration::from_millis(200);
const WAIT: Duration = Duration::from_secs(10);

/// The server's fake: the start lines of `serve`, then it waits.
const SERVE: &str = "echo 'notedown cloud server'
echo 'listening on: 0.0.0.0:19072'
echo 'device URL: http://192.168.1.20:19072'
echo 'REST API: http://192.168.1.20:19072/rest/v1 (connect a pipeline with notedown connect http://192.168.1.20:19072)'
echo \"data folder: $3\"
exec /bin/sleep 311";

/// The pipeline's fake.
const PIPELINE: &str = "exec /bin/sleep 312";

struct Rig {
    dir: tempfile::TempDir,
    spawned: Arc<AtomicUsize>,
}

impl Rig {
    fn new() -> Self {
        let rig = Self {
            dir: tempfile::tempdir().unwrap(),
            spawned: Arc::new(AtomicUsize::new(0)),
        };
        std::fs::create_dir_all(rig.data()).unwrap();
        std::fs::write(rig.dir.path().join("serve.sh"), SERVE).unwrap();
        std::fs::write(rig.dir.path().join("pipeline.sh"), PIPELINE).unwrap();
        rig
    }

    fn data(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn run_dir(&self) -> PathBuf {
        self.dir.path().join("run")
    }

    /// Points `server.toml` at `port` on loopback.
    fn listen_on(&self, port: u16) {
        std::fs::write(
            self.data().join("server.toml"),
            format!("listen = [\"127.0.0.1:{port}\"]\n"),
        )
        .unwrap();
    }

    fn config(&self) -> AppConfig {
        AppConfig {
            mode: Mode::Local,
            data_dir: self.data(),
            ..AppConfig::default()
        }
    }

    /// Runs `serve.sh` or `pipeline.sh` after the subcommand, and counts the spawns.
    fn processes(&self, sidecar: &Path) -> Processes {
        let dir = self.dir.path().to_path_buf();
        let spawned = self.spawned.clone();
        let spawn: SpawnFn = Arc::new(move |args: &[String]| {
            spawned.fetch_add(1, Ordering::SeqCst);
            let script = if args[0] == "serve" {
                "serve.sh"
            } else {
                "pipeline.sh"
            };
            let mut cmd = Command::new("/bin/sh");
            cmd.arg(dir.join(script)).args(args);
            cmd
        });
        Processes::new(Options {
            spawn,
            clock: system_clock(),
            sidecar: sidecar.to_path_buf(),
            run_dir: self.run_dir(),
            parent: std::process::id(),
            server_grace: GRACE,
            pipeline_grace: GRACE,
        })
    }
}

fn wait_until(mut done: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + WAIT;
    while Instant::now() < until {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    done()
}

fn wait_for(sup: &ProcessSupervisor, want: impl Fn(&ChildState) -> bool) -> ChildState {
    let mut last = sup.state();
    assert!(
        wait_until(|| {
            last = sup.state();
            want(&last)
        }),
        "{} stayed {last:?}",
        sup.kind().name()
    );
    last
}

fn running_pid(sup: &ProcessSupervisor) -> u32 {
    match wait_for(sup, |s| matches!(s, ChildState::Running { .. })) {
        ChildState::Running { pid, .. } => pid,
        _ => unreachable!(),
    }
}

fn outside(what: &str) -> ChildState {
    ChildState::Outside {
        what: what.to_string(),
    }
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// A `/bin/sleep` of an "earlier run": no child of the test, in its own group.
fn orphan_sleep() -> (u32, u32) {
    crate::pidfile::tests::orphan_sleep(313)
}

fn kill_group(pgid: u32) {
    crate::pidfile::tests::kill_group(pgid);
}

#[test]
fn our_own_orphan_is_stopped_and_restarted() {
    let rig = Rig::new();
    rig.listen_on(free_port());
    let (orphan, pgid) = orphan_sleep();
    let path = pidfile::path(&rig.run_dir(), ChildKind::Server);
    pidfile::write(
        &path,
        &PidFile {
            pid: orphan,
            pgid,
            started_ms: 1,
            exe: "/bin/sleep".into(),
        },
    )
    .unwrap();

    // The sidecar is /bin/sleep, so the orphan is ours.
    let p = rig.processes(Path::new("/bin/sleep"));
    p.start_all(&rig.config());
    let gone = wait_until(|| !alive(orphan));
    kill_group(pgid);
    assert!(gone, "the orphan still runs");

    let pid = running_pid(p.server());
    assert_ne!(pid, orphan);
    running_pid(p.pipeline());
    let pf = pidfile::read(&path).expect("no pid file for the new server");
    assert_eq!(pf.pid, pid);
    assert_eq!(pf.pgid, pid);
    assert_eq!(pf.exe, PathBuf::from("/bin/sleep"));
    assert!(pidfile::read(&pidfile::path(&rig.run_dir(), ChildKind::Pipeline)).is_some());

    p.stop_all();
    assert!(
        pidfile::read(&path).is_none(),
        "the pid file outlived the stop"
    );
}

#[test]
fn a_foreign_process_stays_outside() {
    let rig = Rig::new();
    let other = TestServer::start(Some(notedown_401()));
    rig.listen_on(other.port);

    let p = rig.processes(Path::new("/bin/sleep"));
    p.start_all(&rig.config());
    assert_eq!(p.server().state(), outside("server"));
    running_pid(p.pipeline());
    assert_eq!(rig.spawned.load(Ordering::SeqCst), 1, "only the pipeline");

    // Still there: a probe again changes nothing.
    p.reprobe(&rig.config());
    assert_eq!(p.server().state(), outside("server"));
    assert_eq!(rig.spawned.load(Ordering::SeqCst), 1);
}

#[test]
fn a_pid_file_of_a_stranger_does_not_make_it_ours() {
    let rig = Rig::new();
    let other = TestServer::start(Some(notedown_401()));
    rig.listen_on(other.port);
    let (stranger, pgid) = orphan_sleep();
    let path = pidfile::path(&rig.run_dir(), ChildKind::Server);
    pidfile::write(
        &path,
        &PidFile {
            pid: stranger,
            pgid,
            started_ms: 1,
            exe: "/bin/sleep".into(),
        },
    )
    .unwrap();

    // The sidecar is another program: the sleep is not ours and is left alone.
    let p = rig.processes(Path::new("/bin/cat"));
    p.start_all(&rig.config());
    let still = alive(stranger);
    kill_group(pgid);
    assert!(still, "a process that is not ours was stopped");
    assert!(pidfile::read(&path).is_none(), "the stale pid file stayed");
    assert_eq!(p.server().state(), outside("server"));
}

#[test]
fn outside_clears_when_the_port_frees() {
    let rig = Rig::new();
    let other = TestServer::start(Some(notedown_401()));
    rig.listen_on(other.port);

    let p = rig.processes(Path::new("/bin/sleep"));
    p.start_all(&rig.config());
    assert_eq!(p.server().state(), outside("server"));

    drop(other);
    p.reprobe(&rig.config());
    running_pid(p.server());
}

#[test]
fn an_outside_pipeline_waits_for_the_writer_lock() {
    let rig = Rig::new();
    rig.listen_on(free_port());
    let lock = rig.data().join("pipeline").join(".writer.lock");
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    let writer = std::fs::File::create(&lock).unwrap();
    writer.try_lock().unwrap();

    let p = rig.processes(Path::new("/bin/sleep"));
    p.start_all(&rig.config());
    assert_eq!(p.pipeline().state(), outside("pipeline"));
    running_pid(p.server());

    p.reprobe(&rig.config());
    assert_eq!(p.pipeline().state(), outside("pipeline"));

    writer.unlock().unwrap();
    p.reprobe(&rig.config());
    running_pid(p.pipeline());
}

#[test]
fn a_stop_is_not_undone_by_a_probe() {
    let rig = Rig::new();
    let other = TestServer::start(Some(notedown_401()));
    rig.listen_on(other.port);
    let p = rig.processes(Path::new("/bin/sleep"));
    p.start_all(&rig.config());
    assert_eq!(p.server().state(), outside("server"));
    p.stop_all();
    drop(other);
    p.reprobe(&rig.config());
    assert_eq!(p.server().state(), ChildState::Stopped);
    assert_eq!(p.pipeline().state(), ChildState::Stopped);
}

#[test]
fn nothing_starts_after_quit() {
    let rig = Rig::new();
    rig.listen_on(free_port());
    let p = rig.processes(Path::new("/bin/sleep"));
    p.start_all(&rig.config());
    let server = running_pid(p.server());
    let pipeline = running_pid(p.pipeline());
    p.quit();
    assert_eq!(p.server().state(), ChildState::Stopped);
    assert_eq!(p.pipeline().state(), ChildState::Stopped);
    assert!(!alive(server) && !alive(pipeline));
    p.start_all(&rig.config());
    assert_eq!(p.server().state(), ChildState::Stopped);
    assert_eq!(rig.spawned.load(Ordering::SeqCst), 2);
}

#[test]
fn the_serve_info_follows_the_server() {
    let rig = Rig::new();
    rig.listen_on(free_port());
    let p = rig.processes(Path::new("/bin/sleep"));
    p.start_all(&rig.config());
    running_pid(p.server());
    let data = rig.data();
    assert!(
        wait_until(|| p.serve_info().data_dir.as_deref() == Some(data.as_path())),
        "no serve info: {:?}",
        p.serve_info()
    );
    let info = p.serve_info();
    assert_eq!(info.listen, vec!["0.0.0.0:19072".parse().unwrap()]);
    assert_eq!(info.device_urls, vec!["http://192.168.1.20:19072"]);
    assert_eq!(
        serve_info::local_rest_base(&info, 1),
        "http://127.0.0.1:19072/rest/v1"
    );
    p.stop_all();
    assert_eq!(p.serve_info(), ServeInfo::default());
}

#[test]
fn remote_mode_reclaims_and_runs_no_server() {
    let rig = Rig::new();
    let (orphan, pgid) = orphan_sleep();
    pidfile::write(
        &pidfile::path(&rig.run_dir(), ChildKind::Server),
        &PidFile {
            pid: orphan,
            pgid,
            started_ms: 1,
            exe: "/bin/sleep".into(),
        },
    )
    .unwrap();
    let p = rig.processes(Path::new("/bin/sleep"));
    let config = AppConfig {
        mode: Mode::Remote,
        ..rig.config()
    };
    p.start_all(&config);
    let gone = wait_until(|| !alive(orphan));
    kill_group(pgid);
    assert!(gone, "the orphan server of an earlier Local run still runs");
    assert_eq!(p.server().state(), ChildState::Stopped);
    assert_eq!(p.pipeline().state(), ChildState::NotConnected);
}
