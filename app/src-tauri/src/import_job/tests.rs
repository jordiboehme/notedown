use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;
use crate::config::{AppConfig, Mode};
use crate::process::spawn_program;
use crate::rest::test_http::{Scripted, answer};
use crate::test_procs::{gone, kill_in, left_in, tagged_sleep};
use crate::token::{MemoryStore, Token, TokenStore};

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
    }
    fn start(&self) {
        self.note("start");
    }
    fn poll_now(&self) {
        self.note("poll");
    }
    fn imported(&self, _quitting: &dyn Fn() -> bool) {
        self.note("imported");
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
        host,
    );
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
    jobs.pause();
    assert!(gone(dir.path()), "{}", left_in(dir.path()));
    wait_idle(&jobs);
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !events.lock().unwrap().iter().any(is_end),
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
        vec![Some("importing".into()), Some("verifying".into())]
    );
    wait_idle(&jobs);
    let calls = calls(dir.path());
    assert_eq!(calls.len(), 4, "{calls:?}");
    assert_eq!(calls[0], "stop");
    assert!(calls[1].starts_with("cloud import "), "{calls:?}");
    assert_eq!(&calls[2..], ["start", "poll"]);
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
    assert_eq!(&calls[2..], ["start", "poll"]);
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
    jobs.app_data(None, false).unwrap();
    let end = wait_for(&events, is_end);
    assert!(matches!(&end, JobEvent::Done(r) if r.ok && r.kind == JobKind::AppData));
    // The event comes after the mark, so the page reads the config with it.
    let calls = calls(dir.path());
    assert_eq!(calls.len(), 5, "{calls:?}");
    assert_eq!(calls[0], "stop");
    assert!(calls[1].starts_with("cloud import-app-data --empty "));
    assert_eq!(&calls[2..], ["start", "poll", "imported"]);
}

#[test]
fn the_mark_is_the_end_of_the_journal() {
    let dir = tempfile::tempdir().unwrap();
    let state = crate::commands::ConfigState::load(dir.path().to_owned());
    state
        .set_mode(Mode::Local, Some(dir.path().join("data")), None)
        .unwrap();
    state
        .set_setup_record(
            serde_json::from_str(r#"{"backupDir":"/b/backup","snbak":"/b/a.snbak"}"#).unwrap(),
        )
        .unwrap();
    let store = MemoryStore::new();
    // No account: no mark.
    assert_eq!(
        mark_once(&state, "http://127.0.0.1:1/rest/v1", &store),
        Ok(false)
    );
    state
        .try_update(|c| {
            c.account = Some("me".into());
            Ok::<(), ()>(())
        })
        .ok()
        .unwrap();
    store.set("local|me", &Token::new("t0k3n")).unwrap();
    let server = Scripted::start(vec![answer(
        "200 OK",
        r#"{"events":[],"next":1,"end":42,"more":false,"gap":false}"#,
    )]);
    assert_eq!(mark_once(&state, &server.base(), &store), Ok(true));
    let request = server.next();
    assert_eq!(request.target, "/rest/v1/events?from=0&limit=1");
    assert_eq!(request.header("authorization"), Some("Bearer t0k3n"));
    let setup = state.current().setup;
    assert_eq!(
        setup.sync_mark,
        Some(crate::config::SyncMark {
            key: "local|me".into(),
            seq: 42
        })
    );
    // A second mark for the same account is not taken.
    assert_eq!(
        state.take_sync_mark(crate::config::SyncMark {
            key: "local|me".into(),
            seq: 50
        }),
        Ok(false)
    );
    assert_eq!(state.current().setup.sync_mark.unwrap().seq, 42);
    // Both imports are done: the backup choices are cleared.
    assert_eq!(setup.backup_dir, Some(PathBuf::from("/b/backup")));
    let setup = state.imports_done().unwrap().setup;
    assert_eq!((setup.backup_dir, setup.snbak), (None, None));
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
    assert!(!events.lock().unwrap().iter().any(is_end));
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
    assert_eq!(jobs.not_busy(), Ok(()));
    jobs.import(&backup_dir(dir.path()), false, false).unwrap();
    wait_for(&events, is_progress);
    assert_eq!(jobs.not_busy(), Err(WAIT_FOR_IMPORT.to_owned()));
    // A pause is for the backup only: the import runs on.
    jobs.pause();
    assert_eq!(jobs.state(), Some(JobKind::Import));
    jobs.quit();
    let ended = gone(dir.path());
    kill_in(dir.path());
    assert!(ended);
    wait_idle(&jobs);
    assert_eq!(jobs.not_busy(), Ok(()));
    // Each command that touches the server or the login asks the guard first.
    let source = include_str!("../commands.rs");
    for command in [
        "start_processes",
        "set_mode",
        "log_in",
        "log_out",
        "create_account",
        "connect_pipeline",
    ] {
        let start = source
            .find(&format!("pub async fn {command}("))
            .unwrap_or_else(|| panic!("no command {command}"));
        let body = &source[start..];
        let body = &body[..body.find("\n}\n").unwrap()];
        assert!(body.contains("jobs.not_busy()?"), "{command} has no guard");
    }
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
