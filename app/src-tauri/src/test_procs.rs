//! Finds and ends the processes of one test only. A test runs its long sleeps as
//! `<its temp folder>/sleep <n>` (a link to `/bin/sleep`), so the temp folder in the command
//! line tells its processes apart from those of every other test and of the system. No test
//! looks for or kills processes by a pattern that another test could share.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// How long a test waits for a state or an event before it fails. Only a guard against a
/// hang: a correct run never comes near it, also on a loaded machine. No test may rely on it.
pub const HANG_GUARD: Duration = Duration::from_secs(120);

/// Links `<dir>/sleep` to `/bin/sleep` and returns the link. `exec "<dir>/sleep" 316` then
/// shows the folder in its command line, also after the shell is gone.
pub fn tagged_sleep(dir: &Path) -> PathBuf {
    let link = dir.join("sleep");
    if !link.exists() {
        std::os::unix::fs::symlink("/bin/sleep", &link).unwrap();
    }
    link
}

/// The pids whose command line names `dir`.
pub fn pids_in(dir: &Path) -> Vec<i32> {
    let pattern: String = dir
        .display()
        .to_string()
        .chars()
        .flat_map(|c| {
            let special = ".[]()*+?{}|^$\\".contains(c);
            special.then_some('\\').into_iter().chain([c])
        })
        .collect();
    let out = Command::new("pgrep")
        .args(["-f", &pattern])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|p| p.parse().ok())
        .filter(|p| *p != std::process::id() as i32)
        .collect()
}

/// Whether every process that names `dir` is gone, after a wait for the system to reap it
/// ([`HANG_GUARD`] at most).
pub fn gone(dir: &Path) -> bool {
    let start = Instant::now();
    loop {
        if pids_in(dir).is_empty() {
            return true;
        }
        if start.elapsed() > HANG_GUARD {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether a process that names `dir` runs with `text` in its command line (`"sleep 321"`: the
/// test's own sleep of that length runs, so the process is past its `exec`).
pub fn runs_in(dir: &Path, text: &str) -> bool {
    let pids: Vec<String> = pids_in(dir).iter().map(i32::to_string).collect();
    if pids.is_empty() {
        return false;
    }
    let out = Command::new("ps")
        .args(["-o", "command=", "-p", &pids.join(",")])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|line| line.contains(text))
}

/// What still runs in `dir`, for a failure message.
pub fn left_in(dir: &Path) -> String {
    let pids: Vec<String> = pids_in(dir).iter().map(i32::to_string).collect();
    if pids.is_empty() {
        return "nothing".to_owned();
    }
    let out = Command::new("ps")
        .args(["-o", "pid,ppid,pgid,stat,command", "-p", &pids.join(",")])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Sends SIGKILL to every process that names `dir`, so a failed test leaves nothing behind.
pub fn kill_in(dir: &Path) {
    for pid in pids_in(dir) {
        // SAFETY: kill only sends a signal; the pid names this test's own folder.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

#[test]
fn only_the_processes_of_the_folder_are_found() {
    let mine = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let mut child = Command::new(tagged_sleep(mine.path()))
        .arg("318")
        .spawn()
        .unwrap();
    let pid = child.id() as i32;
    let start = Instant::now();
    while !pids_in(mine.path()).contains(&pid) {
        assert!(start.elapsed() < HANG_GUARD, "not found");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(pids_in(other.path()).is_empty());
    kill_in(mine.path());
    child.wait().unwrap();
    assert!(gone(mine.path()));
}
