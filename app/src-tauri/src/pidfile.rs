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

/// How far the start time of the process may be from `started_ms`. The supervisor reads its
/// clock just before the spawn, and the system keeps the start to the microsecond.
const START_TOLERANCE_MS: u64 = 3000;

/// Whether the pid file names a live process of ours:
/// - the pid leads its own process group, the one in the file, as the supervisor starts it;
/// - the process started when the file says (so a reused pid is not taken for ours);
/// - it runs the sidecar.
pub fn ours(pf: &PidFile, sidecar: &Path) -> bool {
    let Ok(pid) = i32::try_from(pf.pid) else {
        return false;
    };
    if pid <= 1 || pf.pid != pf.pgid {
        return false;
    }
    // SAFETY: getpgid only reads the group of a process; -1 when there is no such process.
    let pgid = unsafe { libc::getpgid(pid) };
    if pgid < 0 || u32::try_from(pgid).ok() != Some(pf.pgid) {
        return false;
    }
    let Some(start) = start_ms_of(pid) else {
        return false;
    };
    if start.abs_diff(pf.started_ms) > START_TOLERANCE_MS {
        return false;
    }
    let Some(exe) = exe_of(pid) else {
        return false;
    };
    same_file(&exe, sidecar)
}

/// When a process started, ms since the epoch.
#[cfg(target_os = "macos")]
fn start_ms_of(pid: i32) -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: proc_pidinfo writes at most `size` bytes into `info`, which is that large.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if n != size {
        return None;
    }
    // SAFETY: proc_pidinfo filled the whole struct (it returned its size).
    let info = unsafe { info.assume_init() };
    Some(info.pbi_start_tvsec * 1000 + info.pbi_start_tvusec / 1000)
}

/// When a process started; unknown off macOS, so no process counts as ours there.
#[cfg(not(target_os = "macos"))]
fn start_ms_of(_pid: i32) -> Option<u64> {
    None
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

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    /// A `/bin/sleep` of an "earlier run" that is no child of the test: the shell that starts
    /// it ends at once, so the sleep belongs to launchd, as a child of a crashed app would.
    pub(crate) struct Orphan {
        pub pid: u32,
        pub pgid: u32,
        /// The wall clock just before the start, as the supervisor takes it.
        pub started_ms: u64,
    }

    impl Orphan {
        /// The pid file the app would have written for it.
        pub fn pid_file(&self) -> PidFile {
            PidFile {
                pid: self.pid,
                pgid: self.pgid,
                started_ms: self.started_ms,
                exe: "/bin/sleep".into(),
            }
        }

        pub fn kill(&self) {
            kill_group(self.pgid);
        }
    }

    /// Runs `script` in a new process group; it prints the pid of the sleep it leaves behind.
    fn orphan(script: &str) -> Orphan {
        let started_ms = now_ms();
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(script);
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let child = cmd.stdout(std::process::Stdio::piped()).spawn().unwrap();
        let shell = child.id();
        let out = child.wait_with_output().unwrap();
        let pid: u32 = String::from_utf8(out.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The fork runs the shell (or perl) until it has exec'd the sleep.
        let sleep = Path::new("/bin/sleep");
        let ready = wait_until(|| exe_of(pid as i32).as_deref() == Some(sleep));
        // SAFETY: getpgid only reads the group of a process.
        let pgid = unsafe { libc::getpgid(pid as i32) } as u32;
        if !ready {
            kill_group(shell);
            kill_group(pid);
            panic!("the sleep did not start");
        }
        Orphan {
            pid,
            pgid,
            started_ms,
        }
    }

    /// A sleep that leads its own process group, as the supervisor starts its children.
    pub(crate) fn orphan_sleep(secs: u32) -> Orphan {
        orphan(&format!(
            "/usr/bin/perl -e 'setpgrp(0, 0); exec \"/bin/sleep\", \"{secs}\"' \
             </dev/null >/dev/null 2>&1 & echo $!"
        ))
    }

    /// A sleep in the group of the shell that started it: it leads no group.
    fn orphan_in_group(secs: u32) -> Orphan {
        orphan(&format!(
            "/bin/sleep {secs} </dev/null >/dev/null 2>&1 & echo $!"
        ))
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
        let o = orphan_sleep(301);
        assert_eq!(o.pid, o.pgid, "the orphan leads its group");
        let pf = o.pid_file();
        assert!(ours(&pf, &sleep_exe()), "a live sleep is ours as sidecar");
        assert!(!ours(&pf, Path::new("/bin/cat")), "another program is not");
        let other_group = PidFile {
            pgid: o.pgid + 100_000,
            ..pf.clone()
        };
        assert!(!ours(&other_group, &sleep_exe()), "another group is not");
        o.kill();
        assert!(wait_until(|| !alive(o.pid)));
        assert!(!ours(&pf, &sleep_exe()), "a dead pid is not");
    }

    #[test]
    fn a_pid_file_with_another_start_time_is_not_ours() {
        let o = orphan_sleep(304);
        let pf = o.pid_file();
        let near = PidFile {
            started_ms: pf.started_ms + 1500,
            ..pf.clone()
        };
        let long_before = PidFile {
            started_ms: pf.started_ms - 3_600_000,
            ..pf.clone()
        };
        let later = PidFile {
            started_ms: pf.started_ms + 60_000,
            ..pf.clone()
        };
        let results = (
            ours(&pf, &sleep_exe()),
            ours(&near, &sleep_exe()),
            ours(&long_before, &sleep_exe()),
            ours(&later, &sleep_exe()),
        );
        o.kill();
        assert_eq!(
            results,
            (true, true, false, false),
            "(exact, within the tolerance, an hour before, a minute later)"
        );
    }

    #[test]
    fn a_pid_that_leads_no_group_is_not_ours() {
        let o = orphan_in_group(305);
        assert_ne!(o.pid, o.pgid);
        let ok = ours(&o.pid_file(), &sleep_exe());
        o.kill();
        assert!(!ok, "a pid file with pid != pgid was taken for ours");
    }

    #[test]
    fn reclaim_stops_our_child_and_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(dir.path(), ChildKind::Server);
        let o = orphan_sleep(302);
        write(&p, &o.pid_file()).unwrap();
        let got = reclaim(&p, &sleep_exe(), Duration::from_millis(200));
        let gone = wait_until(|| !alive(o.pid));
        o.kill();
        assert_eq!(got, Reclaimed::Stopped { pid: o.pid });
        assert!(gone, "the old child still runs");
        assert!(!p.exists());
    }

    #[test]
    fn reclaim_leaves_a_stranger_alone() {
        let dir = tempfile::tempdir().unwrap();
        let p = path(dir.path(), ChildKind::Pipeline);
        let o = orphan_sleep(303);
        write(&p, &o.pid_file()).unwrap();
        let got = reclaim(&p, Path::new("/bin/cat"), Duration::from_millis(200));
        let still = alive(o.pid);
        o.kill();
        assert_eq!(got, Reclaimed::Stale);
        assert!(still, "a process that is not ours was signalled");
        assert!(!p.exists());
        assert_eq!(
            reclaim(&p, Path::new("/bin/cat"), Duration::from_millis(200)),
            Reclaimed::Nothing
        );
    }
}
