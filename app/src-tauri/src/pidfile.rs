//! A pid file per child, `<app data dir>/run/<server|pipeline>.json`, so the app knows its own
//! children of an earlier run (after a crash, a forced quit or a `tauri dev` restart) and can
//! stop them instead of taking them for another Notedown.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::process::ChildKind;

/// What the app knows of a child it started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PidFile {
    pub pid: u32,
    /// The child's process group (its own pid, as the supervisor starts it in a new group).
    pub pgid: u32,
    /// When the child started, ms since the epoch.
    pub started_ms: u64,
    /// The program the child runs: the sidecar.
    pub exe: PathBuf,
}

/// `<run_dir>/<server|pipeline>.json`.
pub fn path(run_dir: &Path, kind: ChildKind) -> PathBuf {
    run_dir.join(format!("{}.json", kind.name()))
}

/// Writes the pid file: to a temp file first, then renamed over the old one.
pub fn write(path: &Path, pf: &PidFile) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temp = path.with_extension("json.tmp");
    let json = serde_json::to_vec(pf).map_err(io::Error::other)?;
    let written = std::fs::write(&temp, json).and_then(|()| std::fs::rename(&temp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}

/// The pid file at `path`. A missing or broken file is none.
pub fn read(path: &Path) -> Option<PidFile> {
    let text = std::fs::read(path).ok()?;
    serde_json::from_slice(&text).ok()
}

/// Removes the pid file; a missing one is fine.
pub fn remove(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Whether the pid file names a live process of ours: the pid lives, its process group is still
/// the one in the file, and it runs the sidecar.
pub fn ours(pf: &PidFile, sidecar: &Path) -> bool {
    let Ok(pid) = i32::try_from(pf.pid) else {
        return false;
    };
    if pid <= 1 {
        return false;
    }
    // SAFETY: getpgid only reads the group of a process; -1 when there is no such process.
    let pgid = unsafe { libc::getpgid(pid) };
    if pgid < 0 || u32::try_from(pgid).ok() != Some(pf.pgid) {
        return false;
    }
    let Some(exe) = exe_of(pid) else {
        return false;
    };
    same_file(&exe, sidecar)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The program a process runs.
#[cfg(target_os = "macos")]
pub(crate) fn exe_of(pid: i32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let size = u32::try_from(buf.len()).ok()?;
    // SAFETY: proc_pidpath writes at most `size` bytes into `buf`, which is that long.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), size) };
    let n = usize::try_from(n).ok().filter(|n| *n > 0)?;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..n])))
}

/// The program a process runs.
#[cfg(not(target_os = "macos"))]
pub(crate) fn exe_of(pid: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// What [`reclaim`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reclaimed {
    /// There was no pid file.
    Nothing,
    /// The pid file named a child of an earlier run, which is now stopped.
    Stopped { pid: u32 },
    /// The pid file named no child of ours; it is removed.
    Stale,
}

/// How long [`reclaim`] waits for the group to be gone after SIGKILL.
const GONE_WAIT: Duration = Duration::from_secs(5);

/// Stops a child of an earlier run that the pid file at `path` names (SIGTERM to its group,
/// SIGKILL after `grace`), then removes the file. A file that names no child of ours is only
/// removed.
pub fn reclaim(path: &Path, sidecar: &Path, grace: Duration) -> Reclaimed {
    let Some(pf) = read(path) else {
        if path.exists() {
            remove(path);
            return Reclaimed::Stale;
        }
        return Reclaimed::Nothing;
    };
    // Never our own group: the app is no child of itself.
    // SAFETY: getpgrp has no effects.
    let own = u32::try_from(unsafe { libc::getpgrp() }).ok();
    if !ours(&pf, sidecar) || pf.pgid <= 1 || Some(pf.pgid) == own {
        remove(path);
        return Reclaimed::Stale;
    }
    let _ = crate::process::signal_group(pf.pgid, libc::SIGTERM);
    wait_gone(pf.pgid, grace);
    let _ = crate::process::signal_group(pf.pgid, libc::SIGKILL);
    wait_gone(pf.pgid, GONE_WAIT);
    remove(path);
    Reclaimed::Stopped { pid: pf.pid }
}

/// Waits until the group is gone, at most `limit`. The old child is no child of the app; launchd
/// reaps it.
fn wait_gone(pgid: u32, limit: Duration) {
    let until = Instant::now() + limit;
    while crate::process::group_alive(pgid) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::process::Command;

    const WAIT: Duration = Duration::from_secs(10);

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

    fn alive(pid: u32) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    /// A `/bin/sleep` that is no child of the test: the shell that starts it in a new process
    /// group ends at once, so the sleep belongs to launchd, as a child of a crashed app would.
    /// Gives the sleep's pid and its group.
    pub(crate) fn orphan_sleep(secs: u32) -> (u32, u32) {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(format!(
            "/bin/sleep {secs} </dev/null >/dev/null 2>&1 & echo $!"
        ));
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let child = cmd.stdout(std::process::Stdio::piped()).spawn().unwrap();
        let pgid = child.id();
        let out = child.wait_with_output().unwrap();
        let pid: u32 = String::from_utf8(out.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The fork runs the shell until it has exec'd the sleep.
        let sleep = Path::new("/bin/sleep");
        let ready = wait_until(|| exe_of(pid as i32).as_deref() == Some(sleep));
        if !ready {
            kill_group(pgid);
            panic!("the sleep did not start");
        }
        (pid, pgid)
    }

    pub(crate) fn kill_group(pgid: u32) {
        // SAFETY: killpg has no memory effects.
        unsafe { libc::killpg(pgid as i32, libc::SIGKILL) };
    }

    fn sleep_exe() -> PathBuf {
        PathBuf::from("/bin/sleep")
    }

    #[test]
    fn the_pid_file_is_named_after_the_child() {
        let run = Path::new("/x/run");
        assert_eq!(
            path(run, ChildKind::Server),
            PathBuf::from("/x/run/server.json")
        );
        assert_eq!(
            path(run, ChildKind::Pipeline),
            PathBuf::from("/x/run/pipeline.json")
        );
    }

    #[test]
    fn a_pid_file_round_trips_and_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(&dir.path().join("run"), ChildKind::Server);
        let pf = PidFile {
            pid: 42,
            pgid: 42,
            started_ms: 1_700_000_000_000,
            exe: "/Applications/Notedown.app/Contents/MacOS/notedown".into(),
        };
        write(&p, &pf).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(json["pid"], 42);
        assert_eq!(json["pgid"], 42);
        assert_eq!(json["started_ms"], 1_700_000_000_000u64);
        assert_eq!(read(&p), Some(pf));
        remove(&p);
        assert!(!p.exists());
        assert_eq!(read(&p), None);
        remove(&p);
    }

    #[test]
    fn a_broken_pid_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("server.json");
        std::fs::write(&p, "{not json").unwrap();
        assert_eq!(read(&p), None);
    }

    #[test]
    fn ours_needs_a_live_pid_with_the_sidecar() {
        let (pid, pgid) = orphan_sleep(301);
        let pf = PidFile {
            pid,
            pgid,
            started_ms: 0,
            exe: sleep_exe(),
        };
        assert!(ours(&pf, &sleep_exe()), "a live sleep is ours as sidecar");
        assert!(!ours(&pf, Path::new("/bin/cat")), "another program is not");
        let other_group = PidFile {
            pgid: pgid + 100_000,
            ..pf.clone()
        };
        assert!(!ours(&other_group, &sleep_exe()), "another group is not");
        kill_group(pgid);
        assert!(wait_until(|| !alive(pid)));
        assert!(!ours(&pf, &sleep_exe()), "a dead pid is not");
    }

    #[test]
    fn reclaim_stops_our_child_and_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(dir.path(), ChildKind::Server);
        let (pid, pgid) = orphan_sleep(302);
        let pf = PidFile {
            pid,
            pgid,
            started_ms: 0,
            exe: sleep_exe(),
        };
        write(&p, &pf).unwrap();
        let got = reclaim(&p, &sleep_exe(), Duration::from_millis(200));
        let gone = wait_until(|| !alive(pid));
        kill_group(pgid);
        assert_eq!(got, Reclaimed::Stopped { pid });
        assert!(gone, "the old child still runs");
        assert!(!p.exists());
    }

    #[test]
    fn reclaim_leaves_a_stranger_alone() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(dir.path(), ChildKind::Pipeline);
        let (pid, pgid) = orphan_sleep(303);
        let pf = PidFile {
            pid,
            pgid,
            started_ms: 0,
            exe: sleep_exe(),
        };
        write(&p, &pf).unwrap();
        let got = reclaim(&p, Path::new("/bin/cat"), Duration::from_millis(200));
        let still = alive(pid);
        kill_group(pgid);
        assert_eq!(got, Reclaimed::Stale);
        assert!(still, "a process that is not ours was signalled");
        assert!(!p.exists());
        assert_eq!(
            reclaim(&p, Path::new("/bin/cat"), Duration::from_millis(200)),
            Reclaimed::Nothing
        );
    }
}
