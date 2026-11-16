//! Finds and ends the processes of one test only. A test runs its long sleeps as
//! `<its temp folder>/sleep <n>` (a link to `/bin/sleep`), so the temp folder in the command
//! line tells its processes apart from those of every other test and of the system. No test
//! looks for or kills processes by a pattern that another test could share.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

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

/// Whether every process that names `dir` is gone, after a short wait for the system to reap
/// it.
pub fn gone(dir: &Path) -> bool {
    let start = Instant::now();
    loop {
        if pids_in(dir).is_empty() {
            return true;
        }
        if start.elapsed() > Duration::from_secs(3) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
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
        assert!(start.elapsed() < Duration::from_secs(5), "not found");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(pids_in(other.path()).is_empty());
    kill_in(mine.path());
    child.wait().unwrap();
    assert!(gone(mine.path()));
}
