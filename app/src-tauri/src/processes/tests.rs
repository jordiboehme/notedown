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
use crate::pidfile::tests::Orphan;
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

/// A `/bin/sleep` of an "earlier run": no child of the test, leading its own group.
fn orphan_sleep() -> Orphan {
    crate::pidfile::tests::orphan_sleep(313)
}

#[test]
fn our_own_orphan_is_stopped_and_restarted() {
    let rig = Rig::new();
    rig.listen_on(free_port());
    let o = orphan_sleep();
    let path = pidfile::path(&rig.run_dir(), ChildKind::Server);
    pidfile::write(&path, &o.pid_file()).unwrap();

    // The sidecar is /bin/sleep, so the orphan is ours.
    let p = rig.processes(Path::new("/bin/sleep"));
    p.start_all(&rig.config());
    let gone = wait_until(|| !alive(o.pid));
    o.kill();
    assert!(gone, "the orphan still runs");

    let pid = running_pid(p.server());
    assert_ne!(pid, o.pid);
    running_pid(p.pipeline());
    let pf = pidfile::read(&path).expect("no pid file for the new server");
    assert_eq!(pf.pid, pid);
    assert_eq!(pf.pgid, pid);
    assert_eq!(pf.exe, PathBuf::from("/bin/sleep"));
    // What the supervisor writes is ours by the same check a later start makes (pid and
    // group, start time, program): the fake pipeline execs /bin/sleep.
    let pipeline_file = pidfile::path(&rig.run_dir(), ChildKind::Pipeline);
    assert!(
        wait_until(|| pidfile::read(&pipeline_file)
            .is_some_and(|pf| pidfile::ours(&pf, Path::new("/bin/sleep")))),
        "the pipeline's own pid file is not ours: {:?}",
        pidfile::read(&pipeline_file)
    );

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
    let o = orphan_sleep();
    let path = pidfile::path(&rig.run_dir(), ChildKind::Server);
    pidfile::write(&path, &o.pid_file()).unwrap();

    // The sidecar is another program: the sleep is not ours and is left alone.
    let p = rig.processes(Path::new("/bin/cat"));
    p.start_all(&rig.config());
    let still = alive(o.pid);
    o.kill();
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
        serve_info::local_rest_base(&info, "127.0.0.1:1".parse().unwrap()),
        "http://127.0.0.1:19072/rest/v1"
    );
    p.stop_all();
    assert_eq!(p.serve_info(), ServeInfo::default());
}

#[test]
fn a_new_serve_address_is_told() {
    let rig = Rig::new();
    rig.listen_on(free_port());
    let p = rig.processes(Path::new("/bin/sleep"));
    let told = Arc::new(AtomicUsize::new(0));
    let count = told.clone();
    p.on_serve_info(move || {
        count.fetch_add(1, Ordering::SeqCst);
    });
    p.start_all(&rig.config());
    running_pid(p.server());
    let data = rig.data();
    assert!(wait_until(
        || p.serve_info().data_dir.as_deref() == Some(data.as_path())
    ));
    // The address, the device URL, the REST line and the folder each change it; the header
    // of a fresh start changes nothing.
    assert!(wait_until(|| told.load(Ordering::SeqCst) >= 4));
    assert_eq!(told.load(Ordering::SeqCst), 4);
    p.stop_all();
}

#[test]
fn remote_mode_reclaims_and_runs_no_server() {
    let rig = Rig::new();
    let o = orphan_sleep();
    pidfile::write(
        &pidfile::path(&rig.run_dir(), ChildKind::Server),
        &o.pid_file(),
    )
    .unwrap();
    let p = rig.processes(Path::new("/bin/sleep"));
    let config = AppConfig {
        mode: Mode::Remote,
        ..rig.config()
    };
    p.start_all(&config);
    let gone = wait_until(|| !alive(o.pid));
    o.kill();
    assert!(gone, "the orphan server of an earlier Local run still runs");
    assert_eq!(p.server().state(), ChildState::Stopped);
    assert_eq!(p.pipeline().state(), ChildState::NotConnected);
}

#[test]
fn a_start_on_this_mac_makes_the_data_folder_first() {
    let rig = Rig::new();
    // The pipeline fails without its folder, as the real one does.
    std::fs::write(
        rig.dir.path().join("pipeline.sh"),
        "[ -d \"$3\" ] || { echo 'error: no data folder' >&2; exit 1; }\nexec /bin/sleep 312",
    )
    .unwrap();
    let p = rig.processes(Path::new("/bin/sleep"));
    let fresh = rig.dir.path().join("fresh").join("data");
    let config = AppConfig {
        data_dir: fresh.clone(),
        ..rig.config()
    };
    p.start_all(&config);
    assert!(fresh.is_dir(), "the data folder was not made");
    // The pipeline found its folder at its first start. (The server probes the default port
    // here, so it may be Outside on a Mac that runs Notedown; it is not looked at.)
    let pid = running_pid(p.pipeline());
    // A pipeline without its folder is Running for a moment, then in Backoff.
    std::thread::sleep(Duration::from_millis(300));
    let state = p.pipeline().state();
    p.stop_all();
    assert!(
        matches!(state, ChildState::Running { pid: now, .. } if now == pid),
        "{state:?}"
    );
}

#[test]
fn a_start_reads_the_config_inside_its_lock() {
    let rig = Rig::new();
    rig.listen_on(free_port());
    let p = rig.processes(Path::new("/bin/sleep"));
    // The config is read only after the start holds the lock, so an older snapshot cannot
    // win against a newer start or a stop.
    let mut held = None;
    p.start_with(|| {
        held = Some(p.ops.try_lock().is_err());
        rig.config()
    });
    assert_eq!(held, Some(true), "the config was read outside the lock");
    running_pid(p.server());
    let mut held = None;
    p.reprobe_with(|| {
        held = Some(p.ops.try_lock().is_err());
        rig.config()
    });
    assert_eq!(
        held,
        Some(true),
        "the probe read the config outside the lock"
    );
    p.stop_all();
}
