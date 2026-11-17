use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;
use crate::config::{AppConfig, Mode};
use crate::process::spawn_program;
use crate::test_procs::{gone, kill_in, left_in, tagged_sleep};

/// How long a test waits for an event before it fails.
const WAIT: Duration = Duration::from_secs(10);
/// The stop grace in tests.
const GRACE: Duration = Duration::from_millis(200);

/// A fake sidecar: every call is written to `dir/calls`, then `body` runs. `$S` is this test's
/// own sleep (see [`tagged_sleep`]).
fn sidecar(dir: &Path, body: &str) -> SpawnFn {
    let d = dir.display();
    let s = tagged_sleep(dir);
    let s = s.display();
    let script = format!(
        r#"#!/bin/sh
S="{s}"
echo "$*" >> "{d}/calls"
{body}
"#
    );
    let path = dir.join("notedown");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    spawn_program(path)
}

/// A host whose hooks write `stop`, `start`, `poll` and `imported` to the same `calls` file
/// as the fake sidecar, so one file gives the order of everything.
struct FakeHost {
    calls: PathBuf,
    config: Mutex<AppConfig>,
    outside: AtomicBool,
    /// The lock of the jobs, to note whether the import still holds the server at its start.
    server: Mutex<Option<ServerUse>>,
    /// Runs in `stop_all`, after the note: what happens while the server stops.
    on_stop: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl FakeHost {
    fn new(dir: &Path) -> Arc<Self> {
        Arc::new(Self {
            calls: dir.join("calls"),
            config: Mutex::new(AppConfig {
                mode: Mode::Local,
                data_dir: dir.join("data"),
                account: Some("me".into()),
                ..AppConfig::default()
            }),
            outside: AtomicBool::new(false),
            server: Mutex::new(None),
            on_stop: Mutex::new(None),
        })
    }

    fn note(&self, what: &str) {
        let mut text = fs::read_to_string(&self.calls).unwrap_or_default();
        text.push_str(what);
        text.push('\n');
        fs::write(&self.calls, text).unwrap();
    }
}

impl Host for FakeHost {
    fn config(&self) -> AppConfig {
        self.config.lock().unwrap().clone()
    }
    fn outside(&self) -> bool {
        self.outside.load(Ordering::SeqCst)
    }
    fn stop_all(&self) {
        self.note("stop");
        if let Some(hook) = self.on_stop.lock().unwrap().as_ref() {
            hook();
        }
    }
    fn start(&self) {
        let held = self
            .server
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|s| s.try_use().is_err());
        self.note(if held { "start held" } else { "start" });
    }
    fn poll_now(&self) {
        self.note("poll");
    }
    fn files_imported(&self) {
        self.note("mark");
    }
    fn app_data_imported(&self) {
        self.note("cleared");
    }
}

type Events = Arc<Mutex<Vec<JobEvent>>>;

fn jobs(spawn: SpawnFn, host: Arc<FakeHost>) -> (ImportJobs, Events) {
    let events: Events = Arc::default();
    let seen = events.clone();
    let jobs = ImportJobs::new(
        spawn,
        GRACE,
        Arc::new(move |event: &JobEvent| seen.lock().unwrap().push(event.clone())),
        host.clone(),
    );
    *host.server.lock().unwrap() = Some(jobs.server_use());
    (jobs, events)
}

/// Waits until an event matches.
fn wait_for(events: &Events, what: impl Fn(&JobEvent) -> bool) -> JobEvent {
    let start = Instant::now();
    loop {
        if let Some(found) = events.lock().unwrap().iter().find(|e| what(e)) {
            return found.clone();
        }
        assert!(
            start.elapsed() < WAIT,
            "no such event: {:?}",
            events.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits until no job runs.
fn wait_idle(jobs: &ImportJobs) {
    let start = Instant::now();
    while jobs.busy() {
        assert!(start.elapsed() < WAIT, "the job still runs");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn is_progress(e: &JobEvent) -> bool {
    matches!(e, JobEvent::Progress(_))
}

fn is_end(e: &JobEvent) -> bool {
    matches!(e, JobEvent::Done(_) | JobEvent::Failed { .. })
}

fn calls(dir: &Path) -> Vec<String> {
    fs::read_to_string(dir.join("calls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A backup folder of its own, inside the test's folder.
fn backup_dir(dir: &Path) -> PathBuf {
    dir.join("backup")
}

#[test]
fn a_backup_reports_progress_and_the_report() {
    let dir = tempfile::tempdir().unwrap();
    let body = r#"echo '{"files":1,"bytes":100,"path":"Note/a.note"}'
echo 'not json'
echo '{"bytes":300,"path":"Note/b.note","files":2}'
echo '{"report":{"url":"http://sn:8089","time":"2026-11-17T10:00:00Z","complete":true,"files":2,"bytes":300,"errors":[],"date_warnings":0}}'"#;
    let (jobs, events) = jobs(sidecar(dir.path(), body), FakeHost::new(dir.path()));
    jobs.backup("http://sn:8089", &backup_dir(dir.path()), false)
        .unwrap();
    let done = wait_for(&events, is_end);
    let JobEvent::Done(result) = done else {
        panic!("{done:?}")
    };
    assert_eq!(result.kind, JobKind::Backup);
    assert!(result.ok);
    assert_eq!(result.json["files"], 2);
    assert_eq!(result.json["date_warnings"], 0);
    assert_eq!(result.json.get("error"), None);
    let progress: Vec<JobProgress> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            JobEvent::Progress(p) => Some(p.clone()),
            _ => None,
        })
        .collect();
    let files: Vec<(Option<u64>, Option<u64>)> =
        progress.iter().map(|p| (p.files, p.bytes)).collect();
    assert_eq!(files, vec![(Some(1), Some(100)), (Some(2), Some(300))]);
    assert!(progress.iter().all(|p| p.kind == JobKind::Backup));
    wait_idle(&jobs);
    assert_eq!(jobs.state(), None);
    // The backup reads the Supernote: it leaves the server alone.
    let calls = calls(dir.path());
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].starts_with("cloud backup-device http://sn:8089 "));
}

#[test]
fn pause_ends_the_backup_without_failing() {
    let dir = tempfile::tempdir().unwrap();
    let body = r#"echo '{"files":1,"bytes":100,"path":"Note/a.note"}'
exec "$S" 331"#;
    let (jobs, events) = jobs(sidecar(dir.path(), body), FakeHost::new(dir.path()));
    jobs.backup("http://sn:8089", &backup_dir(dir.path()), false)
        .unwrap();
    wait_for(&events, is_progress);
    assert_eq!(jobs.state(), Some(JobKind::Backup));
    // A backup reads the Supernote: it does not hold the server.
    assert!(!jobs.holds_server());
    jobs.pause();
    assert!(gone(dir.path()), "{}", left_in(dir.path()));
    // The event says the next step may start: Resume is not refused.
    wait_for(&events, |e| {
        *e == JobEvent::Paused {
            kind: JobKind::Backup,
        }
    });
    assert_eq!(jobs.state(), None);
    assert!(
        !events.lock().unwrap().iter().any(is_end),
        "{:?}",
        events.lock().unwrap()
    );
}

#[test]
fn a_pause_after_the_end_keeps_the_result() {
    let dir = tempfile::tempdir().unwrap();
    let body = r#"echo '{"report":{"complete":true,"files":3}}'"#;
    let (jobs, events) = jobs(sidecar(dir.path(), body), FakeHost::new(dir.path()));
    // The process has ended, but the job has not noticed yet.
    jobs.hold_after_exit(Duration::from_millis(800));
    jobs.backup("http://sn:8089", &backup_dir(dir.path()), false)
        .unwrap();
    let start = Instant::now();
    while calls(dir.path()).is_empty() {
        assert!(start.elapsed() < WAIT);
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(jobs.state(), Some(JobKind::Backup));
    jobs.pause();
    let end = wait_for(&events, |e| {
        is_end(e) || matches!(e, JobEvent::Paused { .. })
    });
    assert!(
        matches!(&end, JobEvent::Done(r) if r.ok && r.json["files"] == 3),
        "{end:?}"
    );
}

#[test]
fn a_pause_after_the_report_line_is_done_not_paused() {
    let dir = tempfile::tempdir().unwrap();
    // The report is printed, but the process has not ended yet.
    let body = r#"echo '{"report":{"complete":true,"files":4,"errors":[],"stale":[]}}'
echo '{"files":4,"bytes":400,"path":"Note/d.note"}'
exec "$S" 337"#;
    let (jobs, events) = jobs(sidecar(dir.path(), body), FakeHost::new(dir.path()));
    jobs.backup("http://sn:8089", &backup_dir(dir.path()), false)
        .unwrap();
    // The same reader read the report before this progress line.
    wait_for(&events, is_progress);
    jobs.pause();
    let ended = gone(dir.path());
    kill_in(dir.path());
    assert!(ended, "{}", left_in(dir.path()));
    let end = wait_for(&events, |e| {
        is_end(e) || matches!(e, JobEvent::Paused { .. })
    });
    assert!(
        matches!(&end, JobEvent::Done(r) if r.ok && r.json["files"] == 4),
        "{end:?}"
    );
    wait_idle(&jobs);
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, JobEvent::Paused { .. })),
        "{:?}",
        events.lock().unwrap()
    );
}

#[test]
fn resume_passes_resume() {
    let dir = tempfile::tempdir().unwrap();
    let backup = backup_dir(dir.path());
    fs::create_dir_all(backup.join("Note")).unwrap();
    fs::write(
        backup.join("backup.json"),
        r#"{"url":"http://sn:8089","time":"2026-11-17T09:00:00Z","complete":false}"#,
    )
    .unwrap();
    let (jobs, events) = jobs(
        sidecar(dir.path(), r#"echo '{"report":{"complete":true}}'"#),
        FakeHost::new(dir.path()),
    );
    // A folder with a backup that did not finish takes only a resume.
    assert_eq!(
        jobs.backup("http://sn:8089", &backup, false),
        Err(BACKUP_NOT_DONE.to_owned())
    );
    jobs.backup("http://sn:8089", &backup, true).unwrap();
    wait_for(&events, is_end);
    let calls = calls(dir.path());
    assert_eq!(calls.len(), 1, "{calls:?}");
    let args: Vec<&str> = calls[0].split(' ').collect();
    assert!(args.contains(&"--resume"), "{calls:?}");
}

#[test]
fn a_folder_that_is_not_the_apps_backup_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (jobs, _) = jobs(sidecar(dir.path(), "exit 0"), FakeHost::new(dir.path()));
    let backup = backup_dir(dir.path());
    fs::create_dir_all(&backup).unwrap();
    fs::write(backup.join("photo.jpg"), "x").unwrap();
    for resume in [false, true] {
        assert_eq!(
            jobs.backup("http://sn:8089", &backup, resume),
            Err(NOT_EMPTY.to_owned())
        );
    }
    fs::remove_file(backup.join("photo.jpg")).unwrap();
    fs::write(
        backup.join("backup.json"),
        r#"{"time":"2026-11-17T09:00:00Z","complete":true}"#,
    )
    .unwrap();
    for resume in [false, true] {
        assert_eq!(
            jobs.backup("http://sn:8089", &backup, resume),
            Err(BACKUP_DONE_THERE.to_owned())
        );
    }
    // Nothing ran.
    assert!(calls(dir.path()).is_empty());
}

#[test]
fn one_job_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let body = r#"echo '{"files":1,"bytes":100,"path":"Note/a.note"}'
exec "$S" 334"#;
    let host = FakeHost::new(dir.path());
    let (jobs, events) = jobs(sidecar(dir.path(), body), host);
    jobs.backup("http://sn:8089", &backup_dir(dir.path()), false)
        .unwrap();
    wait_for(&events, is_progress);
    let other = dir.path().join("other");
    let busy = Err(ANOTHER_RUNS.to_owned());
    assert_eq!(jobs.backup("http://sn:8089", &other, false), busy);
    assert_eq!(jobs.import_dry_run(&other, false, false), busy);
    assert_eq!(jobs.import(&other, false, false), busy);
    assert_eq!(jobs.app_data_dry_run(None, false), busy);
    assert_eq!(jobs.app_data(None, false), busy);
    // A clone is the same control.
    assert_eq!(jobs.clone().state(), Some(JobKind::Backup));
    jobs.quit();
    assert!(gone(dir.path()), "{}", left_in(dir.path()));
    // Nothing but the first backup ran, and the server was never stopped.
    assert_eq!(calls(dir.path()).len(), 1);
}

#[test]
fn quit_ends_a_running_job() {
    let dir = tempfile::tempdir().unwrap();
    let body = r#"echo '{"phase":"importing","done":1,"total":10}'
exec "$S" 332"#;
    let (jobs, events) = jobs(sidecar(dir.path(), body), FakeHost::new(dir.path()));
    jobs.import_dry_run(&backup_dir(dir.path()), false, false)
        .unwrap();
    wait_for(&events, is_progress);
    jobs.quit();
    assert!(gone(dir.path()), "{}", left_in(dir.path()));
    wait_idle(&jobs);
    assert!(!events.lock().unwrap().iter().any(is_end));
    // Nothing starts after Quit.
    assert!(
        jobs.backup("http://sn:8089", &dir.path().join("b2"), false)
            .is_err()
    );
    assert!(gone(dir.path()));
}

#[test]
fn a_failed_job_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let body = r#"echo "warning: the device is slow" >&2
echo "error: the backup holds no files" >&2
echo "a recovery note" >&2
exit 1"#;
    let (jobs, events) = jobs(sidecar(dir.path(), body), FakeHost::new(dir.path()));
    jobs.import_dry_run(&backup_dir(dir.path()), false, false)
        .unwrap();
    let end = wait_for(&events, is_end);
    assert_eq!(
        end,
        JobEvent::Failed {
            kind: JobKind::ImportDryRun,
            text: "the backup holds no files".into()
        }
    );
    wait_idle(&jobs);
}

#[test]
fn a_dry_run_with_errors_is_done_not_failed() {
    let dir = tempfile::tempdir().unwrap();
    let body = r#"echo '{"dryRun":{"folders":[],"skipped":[],"errors":["x"],"filesPresent":0,"files":3,"bytes":10,"needsReplace":false}}'
echo "error: the backup cannot be imported completely" >&2
exit 1"#;
    let (jobs, events) = jobs(sidecar(dir.path(), body), FakeHost::new(dir.path()));
    jobs.import_dry_run(&backup_dir(dir.path()), false, false)
        .unwrap();
    let end = wait_for(&events, is_end);
    let JobEvent::Done(result) = end else {
        panic!("{end:?}")
    };
    assert_eq!(result.kind, JobKind::ImportDryRun);
    assert!(!result.ok);
    assert_eq!(result.json["errors"][0], "x");
    assert_eq!(result.json["files"], 3);
    assert_eq!(
        result.json["error"],
        "the backup cannot be imported completely"
    );
}

#[test]
fn a_backup_with_errors_gives_its_report() {
    let dir = tempfile::tempdir().unwrap();
    let body = r#"echo '{"files":1,"bytes":100,"path":"Note/a.note"}'
echo '{"report":{"complete":true,"files":1,"errors":[{"path":"Note/b.note","error":"timeout"}]}}'
echo "error: 1 file could not be copied" >&2
exit 1"#;
    let (jobs, events) = jobs(sidecar(dir.path(), body), FakeHost::new(dir.path()));
    jobs.backup("http://sn:8089", &backup_dir(dir.path()), false)
        .unwrap();
    let end = wait_for(&events, is_end);
    let JobEvent::Done(result) = end else {
        panic!("{end:?}")
    };
    assert_eq!(result.kind, JobKind::Backup);
    assert!(!result.ok);
    assert_eq!(result.json["errors"][0]["path"], "Note/b.note");
    assert_eq!(result.json["error"], "1 file could not be copied");
}

#[test]
fn the_import_stops_and_restarts_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let body = r#"case "$2" in
import)
  echo '{"phase":"importing","done":10,"total":10}'
  echo '{"phase":"verifying"}'
  echo '{"imported":{"files":10,"bytes":1000,"foldersCreated":2,"replaced":0}}'
  ;;
import-app-data)
  echo "error: the .snbak file cannot be read" >&2
  exit 1
  ;;
esac"#;
    let host = FakeHost::new(dir.path());
    let (jobs, events) = jobs(sidecar(dir.path(), body), host);
    let backup = backup_dir(dir.path());
    jobs.import(&backup, false, false).unwrap();
    let end = wait_for(&events, is_end);
    let JobEvent::Done(result) = end else {
        panic!("{end:?}")
    };
    assert!(result.ok);
    assert_eq!(result.json["files"], 10);
    let phases: Vec<Option<String>> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            JobEvent::Progress(p) => Some(p.phase.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        phases,
        vec![
            Some("importing".into()),
            Some("verifying".into()),
            Some(RESTARTING.into())
        ]
    );
    wait_idle(&jobs);
    // The mark is taken while the server is still stopped, and the import holds the server
    // until it runs again.
    let calls = calls(dir.path());
    assert_eq!(calls.len(), 5, "{calls:?}");
    assert_eq!(calls[0], "stop");
    assert!(calls[1].starts_with("cloud import "), "{calls:?}");
    assert_eq!(&calls[2..], ["mark", "start held", "poll"]);
    assert!(jobs.use_server().is_ok());
    // Also after a failure, and the mark is only for app data that was imported.
    events.lock().unwrap().clear();
    fs::remove_file(dir.path().join("calls")).unwrap();
    jobs.app_data(Some(&dir.path().join("a.snbak")), false)
        .unwrap();
    let end = wait_for(&events, is_end);
    assert_eq!(
        end,
        JobEvent::Failed {
            kind: JobKind::AppData,
            text: "the .snbak file cannot be read".into()
        }
    );
    wait_idle(&jobs);
    let calls = self::calls(dir.path());
    assert_eq!(calls.len(), 4, "{calls:?}");
    assert_eq!(calls[0], "stop");
    assert!(calls[1].starts_with("cloud import-app-data "), "{calls:?}");
    assert_eq!(&calls[2..], ["start held", "poll"]);
}

#[test]
fn a_sidecar_that_cannot_start_restarts_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let host = FakeHost::new(dir.path());
    let (jobs, events) = jobs(spawn_program(dir.path().join("missing")), host);
    let refused = jobs
        .import(&backup_dir(dir.path()), false, false)
        .unwrap_err();
    assert!(
        refused.starts_with("The notedown program could not start"),
        "{refused}"
    );
    assert_eq!(calls(dir.path()), ["stop", "start held", "poll"]);
    assert!(!jobs.busy());
    assert!(jobs.use_server().is_ok());
    // No progress event: one that arrived after the error would show the step as running.
    assert!(
        events.lock().unwrap().is_empty(),
        "{:?}",
        events.lock().unwrap()
    );
}

#[test]
fn the_mark_is_taken_when_the_import_ends() {
    let dir = tempfile::tempdir().unwrap();
    let body = r#"echo '{"appData":{"dryRun":false,"digests":2,"groups":1,"marks":0,"leftToDevice":[],"withoutGroup":[],"skipped":[],"replaced":0}}'"#;
    let (jobs, events) = jobs(sidecar(dir.path(), body), FakeHost::new(dir.path()));
    // A dry run holds nothing and takes no mark.
    jobs.app_data_dry_run(None, false).unwrap();
    wait_for(&events, is_end);
    wait_idle(&jobs);
    assert_eq!(calls(dir.path()).len(), 1);
    fs::remove_file(dir.path().join("calls")).unwrap();
    events.lock().unwrap().clear();
    // The app data import clears the backup choices; the mark came with the files.
    jobs.app_data(None, false).unwrap();
    let end = wait_for(&events, is_end);
    assert!(matches!(&end, JobEvent::Done(r) if r.ok && r.kind == JobKind::AppData));
    let calls = calls(dir.path());
    assert_eq!(calls.len(), 5, "{calls:?}");
    assert_eq!(calls[0], "stop");
    assert!(calls[1].starts_with("cloud import-app-data --empty "));
    assert_eq!(&calls[2..], ["cleared", "start held", "poll"]);
}

/// Writes `<data>/journal/<name>` with `text`.
fn journal(data: &Path, name: &str, text: &str) {
    fs::create_dir_all(data.join("journal")).unwrap();
    fs::write(data.join("journal").join(name), text).unwrap();
}

#[test]
fn a_torn_tail_longer_than_the_window_keeps_the_line_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    journal(&data, "journal-000000000001.jsonl", "{\"seq\":1}\n");
    // The last append did not finish and is longer than the first window read (64 KiB).
    let torn = format!(
        "{{\"seq\":2}}\n{{\"seq\":3,\"text\":\"{}",
        "c".repeat(100_000)
    );
    journal(&data, "journal-000000000002.jsonl", &torn);
    assert_eq!(journal_end(&data), Some(2));
}

#[test]
fn the_mark_is_the_end_of_the_journal() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    // No journal: no mark.
    assert_eq!(journal_end(&data), None);
    journal(
        &data,
        "journal-000000000001.jsonl",
        "{\"seq\":1,\"ts\":1}\n{\"seq\":2,\"ts\":2}\n",
    );
    journal(&data, ".lock", "");
    assert_eq!(journal_end(&data), Some(2));
    // The newest file counts; a last line without its newline is an append that did not end.
    journal(
        &data,
        "journal-000000000003.jsonl",
        "{\"seq\":3,\"ts\":3,\"account\":\"me\"}\n{\"seq\":4,\"ts\":4}\n{\"seq\":5,\"t",
    );
    assert_eq!(journal_end(&data), Some(4));
    // A newest file without a complete line: the one before it.
    journal(&data, "journal-000000000005.jsonl", "{\"seq\":5");
    assert_eq!(journal_end(&data), Some(4));
    // A last line longer than the first window read.
    let long = format!(
        "{{\"seq\":6,\"text\":\"{}\"}}\n{{\"seq\":7,\"text\":\"{}\"}}\n",
        "a".repeat(10),
        "b".repeat(200_000)
    );
    journal(&data, "journal-000000000006.jsonl", &long);
    assert_eq!(journal_end(&data), Some(7));

    let state = crate::commands::ConfigState::load(dir.path().to_owned());
    state
        .set_mode(Mode::Local, Some(data.clone()), None)
        .unwrap();
    // No account: no mark.
    assert_eq!(take_journal_mark(&state), Ok(false));
    state
        .try_update(|c| {
            c.account = Some("me".into());
            Ok::<(), ()>(())
        })
        .ok()
        .unwrap();
    assert_eq!(take_journal_mark(&state), Ok(true));
    let mark = state.current().setup.sync_mark.unwrap();
    assert_eq!((mark.key.as_str(), mark.seq), ("local|me", 7));
    assert!(mark.at_ms > 1_700_000_000_000, "{}", mark.at_ms);
    // No device synced yet.
    assert_eq!(mark.sync_at_mark, None);
    // An import again takes the new end, with the last sync of the Supernote on disk.
    journal(&data, "journal-000000000008.jsonl", "{\"seq\":8}\n");
    let devices = data.join("accounts/me/devices");
    fs::create_dir_all(&devices).unwrap();
    fs::write(
        devices.join("SN1.json"),
        r#"{"serial":"SN1","token_generation":1,"last_login_ms":5,"last_sync_ms":4242}"#,
    )
    .unwrap();
    assert_eq!(take_journal_mark(&state), Ok(true));
    let mark = state.current().setup.sync_mark.unwrap();
    assert_eq!((mark.seq, mark.sync_at_mark), (8, Some(4242)));
    // A data folder without a journal takes none and keeps the old one.
    state
        .set_mode(Mode::Local, Some(dir.path().join("empty")), None)
        .unwrap();
    state
        .try_update(|c| {
            c.account = Some("me".into());
            Ok::<(), ()>(())
        })
        .ok()
        .unwrap();
    assert_eq!(take_journal_mark(&state), Ok(false));
    assert_eq!(state.current().setup.sync_mark.unwrap().seq, 8);
}

#[test]
fn a_quit_during_the_import_does_not_restart() {
    let dir = tempfile::tempdir().unwrap();
    let body = r#"echo '{"phase":"importing","done":1,"total":10}'
exec "$S" 333"#;
    let (jobs, events) = jobs(sidecar(dir.path(), body), FakeHost::new(dir.path()));
    jobs.import(&backup_dir(dir.path()), true, false).unwrap();
    wait_for(&events, is_progress);
    jobs.quit();
    assert!(gone(dir.path()), "{}", left_in(dir.path()));
    wait_idle(&jobs);
    std::thread::sleep(Duration::from_millis(100));
    let calls = calls(dir.path());
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0], "stop");
    assert!(calls[1].starts_with("cloud import "));
    // No mark, no restart and no event: the app quits.
    let events = events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|e| is_end(e) || matches!(e, JobEvent::Paused { .. })),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, JobEvent::Progress(p) if p.phase.as_deref() == Some(RESTARTING)))
    );
}

#[test]
fn the_import_refuses_an_outside_server() {
    let dir = tempfile::tempdir().unwrap();
    let host = FakeHost::new(dir.path());
    host.outside.store(true, Ordering::SeqCst);
    let (jobs, _) = jobs(sidecar(dir.path(), "exit 0"), host.clone());
    let backup = backup_dir(dir.path());
    let outside = Err(crate::status::OUTSIDE_LINE.to_owned());
    assert_eq!(jobs.import(&backup, false, false), outside);
    assert_eq!(jobs.app_data(None, false), outside);
    assert!(!jobs.busy());
    assert!(calls(dir.path()).is_empty());
    // The imports run only on the server's own Mac, for an account.
    host.outside.store(false, Ordering::SeqCst);
    host.config.lock().unwrap().mode = Mode::Remote;
    let here = Err(IMPORT_HERE_ONLY.to_owned());
    assert_eq!(jobs.import_dry_run(&backup, false, false), here);
    assert_eq!(jobs.import(&backup, false, false), here);
    assert_eq!(jobs.app_data_dry_run(None, false), here);
    assert_eq!(jobs.app_data(None, false), here);
    {
        let mut config = host.config.lock().unwrap();
        config.mode = Mode::Local;
        config.account = None;
    }
    let first = Err(crate::setup::LOG_IN_FIRST.to_owned());
    assert_eq!(jobs.import(&backup, false, false), first);
    assert_eq!(jobs.app_data(None, false), first);
    assert!(calls(dir.path()).is_empty());
}

#[test]
fn busy_blocks_the_commands_that_touch_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let body = r#"echo '{"phase":"importing","done":1,"total":10}'
exec "$S" 335"#;
    let (jobs, events) = jobs(sidecar(dir.path(), body), FakeHost::new(dir.path()));
    assert!(jobs.use_server().is_ok());
    assert!(!jobs.holds_server());
    jobs.import(&backup_dir(dir.path()), false, false).unwrap();
    wait_for(&events, is_progress);
    assert_eq!(jobs.use_server().err(), Some(WAIT_FOR_IMPORT.to_owned()));
    assert!(jobs.holds_server());
    // A pause is for the backup only: the import runs on.
    jobs.pause();
    assert_eq!(jobs.state(), Some(JobKind::Import));
    jobs.quit();
    let ended = gone(dir.path());
    kill_in(dir.path());
    assert!(ended);
    wait_idle(&jobs);
    assert!(jobs.use_server().is_ok());
    assert!(!jobs.holds_server());
    // Each command that starts the children or changes the login holds the guard.
    let source = include_str!("../commands.rs");
    for command in [
        "start_processes",
        "set_mode",
        "log_in",
        "log_out",
        "create_account",
        "connect_pipeline",
        "set_markdown_dir",
    ] {
        let start = source
            .find(&format!("pub async fn {command}("))
            .unwrap_or_else(|| panic!("no command {command}"));
        let body = &source[start..];
        let body = &body[..body.find("\n}\n").unwrap()];
        // Bound to a name: `let _ =` would drop the guard at once.
        assert!(
            body.contains("let _server = jobs.use_server()?;")
                || body.contains("let server = jobs.use_server()?;"),
            "{command} has no guard"
        );
        assert!(
            !body.contains("let _ = jobs.use_server()"),
            "{command} drops its guard at once"
        );
    }
}

#[test]
fn a_command_in_flight_keeps_the_import_out() {
    let dir = tempfile::tempdir().unwrap();
    let body = r#"case "$2" in
backup-device) echo '{"files":1,"bytes":1,"path":"a"}'; exec "$S" 336 ;;
*) echo '{"dryRun":{"files":1}}' ;;
esac"#;
    let (jobs, events) = jobs(sidecar(dir.path(), body), FakeHost::new(dir.path()));
    let backup = backup_dir(dir.path());
    {
        // A command uses the server, for example a login that is still waiting for its answer.
        let _command = jobs.use_server().unwrap();
        assert_eq!(
            jobs.import(&backup, false, false),
            Err(ANOTHER_RUNS.to_owned())
        );
        assert_eq!(jobs.app_data(None, false), Err(ANOTHER_RUNS.to_owned()));
        assert!(!jobs.busy());
        // The steps that leave the server alone may run meanwhile.
        jobs.import_dry_run(&backup, false, false).unwrap();
        wait_for(&events, is_end);
        wait_idle(&jobs);
    }
    assert!(calls(dir.path()).iter().all(|c| c != "stop"));
    // A running backup does not keep the commands out.
    jobs.backup("http://sn:8089", &dir.path().join("b2"), false)
        .unwrap();
    wait_for(&events, is_progress);
    let command = jobs.use_server();
    assert!(command.is_ok());
    // But one step at a time: the import waits for the backup.
    drop(command);
    assert_eq!(
        jobs.import(&backup, false, false),
        Err(ANOTHER_RUNS.to_owned())
    );
    jobs.quit();
    let ended = gone(dir.path());
    kill_in(dir.path());
    assert!(ended, "{}", left_in(dir.path()));
}

#[test]
fn the_error_text_is_the_error_line_or_the_last_line() {
    use std::os::unix::process::ExitStatusExt;
    let failed = ExitStatus::from_raw(1 << 8);
    assert_eq!(
        job_error_text(b"warning: slow\nerror: no files\nnote: x\n", failed),
        "no files"
    );
    // The CLI broke its contract: its last line, never all of them.
    assert_eq!(
        job_error_text(b"warning: slow\nsomething broke\n", failed),
        "something broke"
    );
    assert_eq!(job_error_text(b"", failed), "it ended with exit code 1");
}

#[test]
fn every_run_has_json_and_exit_with_parent() {
    let dir = tempfile::tempdir().unwrap();
    let (jobs, events) = jobs(
        sidecar(dir.path(), r#"echo '{"report":{}}'"#),
        FakeHost::new(dir.path()),
    );
    let backup = backup_dir(dir.path());
    let snbak = dir.path().join("a.snbak");
    let runs: Vec<Box<dyn Fn() -> Result<(), String>>> = vec![
        Box::new(|| jobs.backup("http://sn:8089", &dir.path().join("b1"), false)),
        Box::new(|| jobs.import_dry_run(&backup, true, true)),
        Box::new(|| jobs.import(&backup, false, false)),
        Box::new(|| jobs.app_data_dry_run(Some(&snbak), true)),
        Box::new(|| jobs.app_data(None, false)),
    ];
    for run in runs {
        events.lock().unwrap().clear();
        run().unwrap();
        wait_for(&events, is_end);
        wait_idle(&jobs);
    }
    let tail = format!("--json --exit-with-parent {}", std::process::id());
    let data = dir.path().join("data");
    let data = data.display();
    let b = backup.display();
    let s = snbak.display();
    let runs: Vec<String> = calls(dir.path())
        .into_iter()
        .filter(|c| c.starts_with("cloud "))
        .collect();
    assert_eq!(
        runs,
        vec![
            format!(
                "cloud backup-device http://sn:8089 {} {tail}",
                dir.path().join("b1").display()
            ),
            format!(
                "cloud import {b} --data {data} --account me --dry-run --replace --allow-empty {tail}"
            ),
            format!("cloud import {b} --data {data} --account me {tail}"),
            format!(
                "cloud import-app-data {s} --data {data} --account me --dry-run --replace {tail}"
            ),
            format!("cloud import-app-data --empty --data {data} --account me {tail}"),
        ]
    );
}

#[test]
fn backup_folder_states() {
    let dir = tempfile::tempdir().unwrap();
    let backup = backup_dir(dir.path());
    assert_eq!(backup_folder_state(&backup), BackupFolder::Missing);
    fs::create_dir(&backup).unwrap();
    assert_eq!(backup_folder_state(&backup), BackupFolder::Empty);
    // The backup would refuse a folder with anything in it, so the app does too.
    fs::write(backup.join(".DS_Store"), "x").unwrap();
    assert_eq!(backup_folder_state(&backup), BackupFolder::Foreign);
    fs::remove_file(backup.join(".DS_Store")).unwrap();
    fs::create_dir(backup.join("Note")).unwrap();
    fs::write(
        backup.join("backup.json"),
        r#"{"url":"http://sn:8089","time":"2026-11-17T09:00:00Z","complete":false,"files":3}"#,
    )
    .unwrap();
    assert_eq!(
        backup_folder_state(&backup),
        BackupFolder::Resumable {
            time: "2026-11-17T09:00:00Z".into()
        }
    );
    fs::write(
        backup.join("backup.json"),
        r#"{"url":"http://sn:8089","time":"2026-11-17T10:00:00Z","complete":true,"files":3}"#,
    )
    .unwrap();
    assert_eq!(
        backup_folder_state(&backup),
        BackupFolder::Complete {
            time: "2026-11-17T10:00:00Z".into()
        }
    );
    fs::write(backup.join("backup.json"), "not json").unwrap();
    assert_eq!(backup_folder_state(&backup), BackupFolder::Foreign);
    // A folder with a stray file and no backup.json.
    let stray = dir.path().join("stray");
    fs::create_dir(&stray).unwrap();
    fs::write(stray.join("notes.txt"), "x").unwrap();
    assert_eq!(backup_folder_state(&stray), BackupFolder::Foreign);
    // A folder that cannot be read is not called foreign.
    let locked = dir.path().join("locked");
    fs::create_dir(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let state = backup_folder_state(&locked);
    let (jobs, _) = jobs(sidecar(dir.path(), "exit 0"), FakeHost::new(dir.path()));
    let refused = jobs.backup("http://sn:8089", &locked, false);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(state, BackupFolder::Unreadable);
    assert_eq!(refused, Err(CANNOT_READ.to_owned()));
    // A file is no folder.
    assert_eq!(
        backup_folder_state(&stray.join("notes.txt")),
        BackupFolder::Foreign
    );
    // The page reads the state by these names.
    assert_eq!(
        serde_json::to_value(BackupFolder::Resumable { time: "t".into() }).unwrap(),
        serde_json::json!({"state": "resumable", "time": "t"})
    );
    assert_eq!(
        serde_json::to_value(BackupFolder::Missing).unwrap(),
        serde_json::json!({"state": "missing"})
    );
}

#[test]
fn the_events_carry_the_names_the_page_reads() {
    let progress = JobEvent::Progress(JobProgress {
        kind: JobKind::ImportDryRun,
        files: None,
        bytes: None,
        done: Some(1),
        total: Some(2),
        phase: Some("importing".into()),
    });
    let JobEvent::Progress(p) = progress else {
        unreachable!()
    };
    assert_eq!(
        serde_json::to_value(&p).unwrap(),
        serde_json::json!({"kind": "importDryRun", "files": null, "bytes": null, "done": 1,
            "total": 2, "phase": "importing"})
    );
    let done = JobResult {
        kind: JobKind::AppDataDryRun,
        ok: true,
        json: serde_json::json!({}),
    };
    assert_eq!(
        serde_json::to_value(&done).unwrap(),
        serde_json::json!({"kind": "appDataDryRun", "ok": true, "json": {}})
    );
}

#[test]
fn rfc3339_times_are_read() {
    assert_eq!(rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(rfc3339_ms("2026-11-17T14:05:00Z"), Some(1_794_924_300_000));
    assert_eq!(
        rfc3339_ms("2026-11-17T14:05:00.25Z"),
        Some(1_794_924_300_250)
    );
    assert_eq!(
        rfc3339_ms("2026-11-17T15:05:00+01:00"),
        Some(1_794_924_300_000)
    );
    for bad in ["", "2026-11-17", "2026-13-01T00:00:00Z", "yesterday"] {
        assert_eq!(rfc3339_ms(bad), None, "{bad}");
    }
}

const HOUR_MS: u64 = 3_600_000;
/// 2026-11-17T14:05:00Z.
const MADE: u64 = 1_794_924_300_000;

fn backup_made(dir: &Path, time: &str) {
    fs::write(
        dir.join("backup.json"),
        format!(r#"{{"url":"http://x","time":"{time}","complete":true}}"#),
    )
    .unwrap();
}

#[test]
fn the_import_refuses_an_old_backup() {
    let dir = tempfile::tempdir().unwrap();
    backup_made(dir.path(), "2026-11-17T14:05:00Z");
    assert_eq!(backup_fresh(dir.path(), MADE + HOUR_MS, None), Ok(()));
    assert_eq!(backup_fresh(dir.path(), MADE + 24 * HOUR_MS, None), Ok(()));
    // Older than 24 hours.
    assert_eq!(
        backup_fresh(dir.path(), MADE + 24 * HOUR_MS + 1, None),
        Err(BACKUP_TOO_OLD.to_owned())
    );
    // Not newer than the last device sync: a file missing on the server would be deleted on
    // the Supernote.
    assert_eq!(
        backup_fresh(dir.path(), MADE + HOUR_MS, Some(MADE)),
        Err(BACKUP_TOO_OLD.to_owned())
    );
    assert_eq!(
        backup_fresh(dir.path(), MADE + HOUR_MS, Some(MADE - 1)),
        Ok(())
    );
    // A backup whose time cannot be read is refused as well.
    let other = tempfile::tempdir().unwrap();
    assert_eq!(
        backup_fresh(other.path(), MADE, None),
        Err(BACKUP_TOO_OLD.to_owned())
    );
    backup_made(other.path(), "not a time");
    assert_eq!(
        backup_fresh(other.path(), MADE, None),
        Err(BACKUP_TOO_OLD.to_owned())
    );
}

#[test]
fn the_import_sees_a_device_sync_on_disk_while_the_status_has_none() {
    let dir = tempfile::tempdir().unwrap();
    backup_made(dir.path(), "2026-11-17T14:05:00Z");
    let data = dir.path().join("data");
    let config = AppConfig {
        mode: Mode::Local,
        data_dir: data.clone(),
        account: Some("me".into()),
        ..AppConfig::default()
    };
    // No device file: only the 24 hours count.
    assert_eq!(last_device_sync(&config, None), None);
    // The command uses it.
    let source = include_str!("../commands.rs");
    let start = source.find("pub async fn import_run(").unwrap();
    let body = &source[start..];
    let body = &body[..body.find("\n}\n").unwrap()];
    assert!(body.contains("import_job::last_device_sync("), "{body}");
    let devices = data.join("accounts/me/devices");
    fs::create_dir_all(&devices).unwrap();
    fs::write(
        devices.join("SN1.json"),
        format!(
            r#"{{"serial":"SN1","token_generation":1,"last_login_ms":5,"last_sync_ms":{}}}"#,
            MADE + 60_000
        ),
    )
    .unwrap();
    // The server does not run, so the status has no device: the file on disk still counts.
    let last = last_device_sync(&config, None);
    assert_eq!(last, Some(MADE + 60_000));
    assert_eq!(
        backup_fresh(dir.path(), MADE + HOUR_MS, last),
        Err(BACKUP_TOO_OLD.to_owned())
    );
    // The larger of the two wins.
    assert_eq!(last_device_sync(&config, Some(MADE)), Some(MADE + 60_000));
    assert_eq!(
        last_device_sync(&config, Some(MADE + HOUR_MS)),
        Some(MADE + HOUR_MS)
    );
    // Without an account no device file is read.
    let none = AppConfig {
        account: None,
        ..config
    };
    assert_eq!(last_device_sync(&none, None), None);
}

#[test]
fn a_device_sync_that_ends_while_the_server_stops_refuses_the_import() {
    let dir = tempfile::tempdir().unwrap();
    let backup = backup_dir(dir.path());
    fs::create_dir_all(&backup).unwrap();
    backup_made(&backup, "2026-11-17T14:05:00Z");
    let host = FakeHost::new(dir.path());
    // The Supernote's sync ends while the server stops: the device file on disk is newer.
    let devices = dir.path().join("data/accounts/me/devices");
    *host.on_stop.lock().unwrap() = Some(Box::new(move || {
        fs::create_dir_all(&devices).unwrap();
        fs::write(
            devices.join("SN1.json"),
            format!(
                r#"{{"serial":"SN1","token_generation":1,"last_login_ms":5,"last_sync_ms":{}}}"#,
                MADE + 60_000
            ),
        )
        .unwrap();
    }));
    let (jobs, events) = jobs(sidecar(dir.path(), "exit 0"), host);
    assert_eq!(
        jobs.import(&backup, false, false),
        Err(BACKUP_TOO_OLD.to_owned())
    );
    // The program never ran, and the server started again.
    assert_eq!(calls(dir.path()), ["stop", "start held", "poll"]);
    assert!(!jobs.busy());
    assert!(jobs.use_server().is_ok());
    assert!(
        events.lock().unwrap().is_empty(),
        "{:?}",
        events.lock().unwrap()
    );
    // Without a newer sync the import starts.
    assert_eq!(backup_synced_before(&backup, Some(MADE - 1)), Ok(()));
    assert_eq!(backup_synced_before(&backup, None), Ok(()));
}

#[test]
fn the_app_data_import_refuses_an_old_snbak() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Supernote_1.snbak");
    fs::write(&path, b"x").unwrap();
    let made = std::time::UNIX_EPOCH + Duration::from_millis(MADE);
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(made)
        .unwrap();
    assert_eq!(snbak_fresh(&path, MADE + 23 * HOUR_MS), Ok(()));
    assert_eq!(
        snbak_fresh(&path, MADE + 25 * HOUR_MS),
        Err(SNBAK_TOO_OLD.to_owned())
    );
    assert!(snbak_fresh(&dir.path().join("gone.snbak"), MADE).is_err());
}

#[test]
fn the_mark_keeps_the_newest_device_sync_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(newest_device_sync(dir.path(), "me"), None);
    let devices = dir.path().join("accounts/me/devices");
    fs::create_dir_all(&devices).unwrap();
    fs::write(
        devices.join("SN1.json"),
        r#"{"serial":"SN1","token_generation":1,"last_login_ms":5,"last_sync_ms":700}"#,
    )
    .unwrap();
    fs::write(
        devices.join("SN2.json"),
        r#"{"serial":"SN2","token_generation":1,"last_login_ms":5,"last_sync_ms":900}"#,
    )
    .unwrap();
    // Never synced, and a broken file: neither counts.
    fs::write(
        devices.join("SN3.json"),
        r#"{"serial":"SN3","token_generation":1,"last_login_ms":5}"#,
    )
    .unwrap();
    fs::write(devices.join("SN4.json"), "{").unwrap();
    assert_eq!(newest_device_sync(dir.path(), "me"), Some(900));
    assert_eq!(newest_device_sync(dir.path(), "anna"), None);
}
