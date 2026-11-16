use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;
use crate::process::spawn_program;

/// How long a test waits for an event before it fails.
const WAIT: Duration = Duration::from_secs(10);
/// The stop grace in tests.
const GRACE: Duration = Duration::from_millis(200);

/// A fake sidecar: `model path` names files in `dir`, `model verify` is ok when the model file
/// is there, `model pull --json` runs `pull`. Every call is written to `dir/calls`.
fn sidecar(dir: &Path, pull: &str) -> SpawnFn {
    let d = dir.display();
    let script = format!(
        r#"#!/bin/sh
echo "$*" >> "{d}/calls"
case "$1 $2" in
"model path")
  if [ -f "{d}/slow-path" ]; then sleep 0.315; fi
  echo "cache: {d}"
  echo "model: {d}/m.gguf"
  echo "mmproj: {d}/p.gguf"
  ;;
"model verify")
  if [ -f "{d}/slow-verify" ]; then exec sleep 310; fi
  if [ -f "{d}/m.gguf" ]; then echo "m.gguf: ok"; exit 0; fi
  echo "m.gguf: the model file is missing"; exit 1
  ;;
"model pull")
{pull}
  ;;
esac
"#
    );
    let path = dir.join("notedown");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    spawn_program(path)
}

type Events = Arc<Mutex<Vec<ModelEvent>>>;

fn control(spawn: SpawnFn) -> (ModelControl, Events) {
    let events: Events = Arc::default();
    let seen = events.clone();
    let control = ModelControl::new(
        spawn,
        GRACE,
        Arc::new(move |event: &ModelEvent| seen.lock().unwrap().push(event.clone())),
    );
    (control, events)
}

/// Waits until an event matches.
fn wait_for(events: &Events, what: impl Fn(&ModelEvent) -> bool) -> ModelEvent {
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

/// Whether a process with this command line is still there, after a short wait for the
/// system to reap it.
fn gone(pattern: &str) -> bool {
    let start = Instant::now();
    loop {
        let found = std::process::Command::new("pgrep")
            .args(["-f", pattern])
            .output()
            .unwrap()
            .status
            .success();
        if !found {
            return true;
        }
        if start.elapsed() > Duration::from_secs(3) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn is_progress(e: &ModelEvent) -> bool {
    matches!(e, ModelEvent::Progress(_))
}

#[test]
fn model_progress_lines_are_events() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display();
    let pull = format!(
        r#"  echo '{{"file":"m.gguf","done":10,"total":100}}'
  echo 'not json'
  echo '{{"file":"m.gguf","done":100,"total":100}}'
  echo x > "{d}/m.gguf"
  echo '{{"ready":"{d}/m.gguf"}}'"#
    );
    let (control, events) = control(sidecar(dir.path(), &pull));
    control.pull().unwrap();
    wait_for(&events, |e| matches!(e, ModelEvent::Ready(_)));
    let progress: Vec<Progress> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            ModelEvent::Progress(p) => Some(p.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        progress,
        vec![
            Progress {
                file: "m.gguf".into(),
                done: 10,
                total: 100
            },
            Progress {
                file: "m.gguf".into(),
                done: 100,
                total: 100
            },
        ]
    );
    assert!(!control.state().pulling);
}

#[test]
fn a_second_pull_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let pull = r#"  echo '{"file":"m.gguf","done":1,"total":100}'
  exec sleep 309"#;
    let (control, events) = control(sidecar(dir.path(), pull));
    control.pull().unwrap();
    wait_for(&events, is_progress);
    let state = control.state();
    assert!(state.pulling);
    assert_eq!(
        state.progress,
        Some(Progress {
            file: "m.gguf".into(),
            done: 1,
            total: 100
        })
    );
    assert_eq!(control.pull(), Err(PULL_RUNS.to_owned()));
    assert_eq!(control.remove().unwrap_err(), PULL_RUNS);
    control.pause();
    assert!(gone("sleep 309"));
}

#[test]
fn pause_stops_the_pull() {
    let dir = tempfile::tempdir().unwrap();
    let pull = r#"  echo '{"file":"m.gguf","done":1,"total":100}'
  exec sleep 306"#;
    let (control, events) = control(sidecar(dir.path(), pull));
    control.pull().unwrap();
    wait_for(&events, is_progress);
    control.pause();
    assert!(gone("sleep 306"), "the pull still runs");
    assert!(!control.state().pulling);
    // A pause is no failure.
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, ModelEvent::Failed { .. })),
        "{:?}",
        events.lock().unwrap()
    );
    // Download again resumes: a new pull starts.
    events.lock().unwrap().clear();
    control.pull().unwrap();
    wait_for(&events, is_progress);
    control.pause();
    assert!(gone("sleep 306"));
}

#[test]
fn quit_stops_the_pull() {
    let dir = tempfile::tempdir().unwrap();
    let pull = r#"  echo '{"file":"m.gguf","done":1,"total":100}'
  exec sleep 308"#;
    let (control, events) = control(sidecar(dir.path(), pull));
    control.pull().unwrap();
    wait_for(&events, is_progress);
    control.quit();
    assert!(gone("sleep 308"), "the pull still runs");
    assert!(!control.state().pulling);
    // Nothing starts after Quit.
    assert!(control.pull().is_err());
    assert!(gone("sleep 308"));
}

#[test]
fn a_failed_pull_reports_its_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let pull = r#"  echo "warning: slow connection" >&2
  echo "error: the connection broke" >&2
  exit 1"#;
    let (control, events) = control(sidecar(dir.path(), pull));
    control.pull().unwrap();
    let failed = wait_for(&events, |e| matches!(e, ModelEvent::Failed { .. }));
    assert_eq!(
        failed,
        ModelEvent::Failed {
            text: "the connection broke".into()
        }
    );
    assert!(!control.state().pulling);
}

#[test]
fn ready_checks_the_model_again() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display();
    let pull = format!(
        r#"  printf 12345 > "{d}/m.gguf"
  printf 678 > "{d}/p.gguf"
  echo '{{"ready":"{d}/m.gguf"}}'"#
    );
    let (control, events) = control(sidecar(dir.path(), &pull));
    control.pull().unwrap();
    let ready = wait_for(&events, |e| matches!(e, ModelEvent::Ready(_)));
    let ModelEvent::Ready(state) = ready else {
        unreachable!()
    };
    assert!(state.installed);
    assert!(!state.pulling);
    assert_eq!(state.size_bytes, 8);
    assert_eq!(
        state.path.as_deref(),
        Some(dir.path().join("m.gguf").to_str().unwrap())
    );
    let calls = fs::read_to_string(dir.path().join("calls")).unwrap();
    let pull_at = calls.find("model pull --json").expect("no pull");
    let verify_at = calls.rfind("model verify").expect("no verify");
    assert!(verify_at > pull_at, "{calls}");
}

#[test]
fn the_state_reads_the_model_files() {
    let dir = tempfile::tempdir().unwrap();
    let (control, _) = control(sidecar(dir.path(), "  exit 0"));
    let none = control.state();
    assert!(!none.installed);
    assert_eq!(none.size_bytes, 0);
    assert_eq!(none.problem, None);
    // A part of a download counts toward the size.
    fs::write(dir.path().join("m.gguf.part"), "1234").unwrap();
    assert_eq!(control.state().size_bytes, 4);
    fs::write(dir.path().join("m.gguf"), "123456").unwrap();
    let some = control.state();
    assert!(some.installed);
    assert_eq!(some.size_bytes, 10);
}

#[test]
fn remove_deletes_only_the_model_files() {
    let dir = tempfile::tempdir().unwrap();
    let (control, _) = control(sidecar(dir.path(), "  exit 0"));
    for name in [
        "m.gguf",
        "p.gguf",
        "m.gguf.part",
        "p.gguf.part",
        "other.txt",
    ] {
        fs::write(dir.path().join(name), "x").unwrap();
    }
    let state = control.remove().unwrap();
    assert!(!state.installed);
    assert_eq!(state.size_bytes, 0);
    for name in ["m.gguf", "p.gguf", "m.gguf.part", "p.gguf.part"] {
        assert!(!dir.path().join(name).exists(), "{name} is still there");
    }
    assert!(dir.path().join("other.txt").exists());
}

#[test]
fn a_sidecar_that_cannot_answer_is_a_problem() {
    let dir = tempfile::tempdir().unwrap();
    let (control, _) = control(spawn_program(dir.path().join("missing")));
    let state = control.state();
    assert!(!state.installed);
    assert!(state.problem.is_some());
    assert!(control.pull().is_err());
}

/// Waits until the fake sidecar was called with `what`.
fn called(dir: &Path, what: &str) {
    let start = Instant::now();
    while !fs::read_to_string(dir.join("calls")).is_ok_and(|c| c.contains(what)) {
        assert!(start.elapsed() < WAIT, "no call of {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn no_pull_starts_while_the_model_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let (control, _) = control(sidecar(dir.path(), "  exit 0"));
    fs::write(dir.path().join("slow-path"), "").unwrap();
    fs::write(dir.path().join("m.gguf"), "x").unwrap();
    let remover = control.clone();
    let removing = std::thread::spawn(move || remover.remove());
    called(dir.path(), "model path");
    assert_eq!(control.pull(), Err(REMOVE_RUNS.to_owned()));
    assert_eq!(control.remove().unwrap_err(), REMOVE_RUNS);
    let state = removing.join().unwrap().unwrap();
    assert!(!state.installed);
    assert!(!dir.path().join("m.gguf").exists());
}

#[test]
fn progress_after_a_pause_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    // The pull still says something while it ends.
    let pull = r#"  trap 'echo "{\"file\":\"m.gguf\",\"done\":2,\"total\":100}"; exit 0' TERM
  sleep 310 &
  echo '{"file":"m.gguf","done":1,"total":100}'
  wait"#;
    let (control, events) = control(sidecar(dir.path(), pull));
    control.pull().unwrap();
    wait_for(&events, is_progress);
    control.pause();
    std::thread::sleep(Duration::from_millis(100));
    let progress = events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| is_progress(e))
        .count();
    assert_eq!(progress, 1, "{:?}", events.lock().unwrap());
    let state = control.state();
    assert!(!state.pulling);
    assert_eq!(state.progress, None);
    assert!(gone("sleep 310"));
}

#[test]
fn quit_stops_a_running_check() {
    let dir = tempfile::tempdir().unwrap();
    let (control, _) = control(sidecar(dir.path(), "  exit 0"));
    fs::write(dir.path().join("slow-verify"), "").unwrap();
    let checker = control.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(checker.state());
    });
    called(dir.path(), "model verify");
    control.quit();
    let ended = rx.recv_timeout(Duration::from_secs(3));
    // Never leave the fake behind, also when the check fails.
    let _ = std::process::Command::new("pkill")
        .args(["-f", "sleep 310"])
        .status();
    let state = ended.expect("the check still runs after Quit");
    assert!(!state.installed);
    assert!(state.problem.is_some());
    // After Quit no check starts.
    assert!(control.state().problem.is_some());
}
