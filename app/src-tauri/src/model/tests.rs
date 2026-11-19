use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;
use crate::process::spawn_program;
use crate::test_procs::{gone, kill_in, left_in, tagged_sleep};

/// How long a test waits for an event before it fails: only a guard against a hang.
const WAIT: Duration = crate::test_procs::HANG_GUARD;
/// The stop grace in tests.
const GRACE: Duration = Duration::from_millis(200);

/// A fake sidecar: `model path` names four files in `dir`, `model verify` is ok when the model
/// file is there, `model pull --json --licences-only` prints two licence lines, `model clean-old`
/// deletes the two old files, `model pull --json` runs `pull`. Every call is written to `dir/calls`. `$S` is
/// this test's own sleep (see [`tagged_sleep`]). With `dir/hold-path`, `model path` waits for
/// `dir/release` (about two minutes at most: only a guard against a hang).
fn sidecar(dir: &Path, pull: &str) -> SpawnFn {
    let d = dir.display();
    let s = tagged_sleep(dir);
    let s = s.display();
    let script = format!(
        r#"#!/bin/sh
S="{s}"
echo "$*" >> "{d}/calls"
case "$1 $2" in
"model path")
  if [ -f "{d}/hold-path" ]; then
    i=0
    while [ ! -f "{d}/release" ] && [ "$i" -lt 12000 ]; do sleep 0.01; i=$((i + 1)); done
  fi
  echo "cache: {d}"
  echo "model: {d}/m.gguf"
  echo "mmproj: {d}/p.gguf"
  echo "layout: {d}/l.safetensors"
  echo "lines: {d}/d.onnx"
  ;;
"model verify")
  if [ -f "{d}/slow-verify" ]; then exec "$S" 316; fi
  if [ -f "{d}/m.gguf" ]; then echo "m.gguf: ok"; exit 0; fi
  echo "m.gguf: the model file is missing"; exit 1
  ;;
"model clean-old")
  rm -f "{d}/{old_model}" "{d}/{old_mmproj}"
  ;;
"model pull")
  if [ "$3 $4" = "--json --licences-only" ]; then
    echo '{{"files":[{{"file":"q.gguf","licence":"Apache-2.0","licensor":"Qwen team","model":"Qwen3.5-4B","page":"https://huggingface.co/Qwen/Qwen3.5-4B"}},{{"file":"m.gguf","licence":"Apache-2.0","licensor":"Qwen team","model":"Qwen3.5-4B","page":"https://huggingface.co/Qwen/Qwen3.5-4B"}},{{"file":"l.safetensors","licence":"Apache-2.0","licensor":"PaddlePaddle Authors","model":"PP-DocLayoutV3","page":"https://huggingface.co/PaddlePaddle/PP-DocLayoutV3"}}],"licences":["Qwen3.5-4B (Qwen team): Apache-2.0","PP-DocLayoutV3 (PaddlePaddle Authors): Apache-2.0"]}}'
    exit 0
  fi
{pull}
  ;;
esac
"#,
        old_model = OLD_FILES[0],
        old_mmproj = OLD_FILES[1],
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
    // The pull ends by itself when the app dies.
    let calls = fs::read_to_string(dir.path().join("calls")).unwrap();
    let pull = format!(
        "model pull --json --exit-with-parent {}",
        std::process::id()
    );
    assert!(calls.lines().any(|l| l == pull), "{calls}");
}

#[test]
fn a_second_pull_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let pull = r#"  echo '{"file":"m.gguf","done":1,"total":100}'
  exec "$S" 309"#;
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
    assert!(gone(dir.path()));
}

#[test]
fn pause_stops_the_pull() {
    let dir = tempfile::tempdir().unwrap();
    let pull = r#"  echo '{"file":"m.gguf","done":1,"total":100}'
  exec "$S" 306"#;
    let (control, events) = control(sidecar(dir.path(), pull));
    control.pull().unwrap();
    wait_for(&events, is_progress);
    control.pause();
    assert!(gone(dir.path()), "the pull still runs");
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
    assert!(gone(dir.path()));
}

#[test]
fn quit_stops_the_pull() {
    let dir = tempfile::tempdir().unwrap();
    let pull = r#"  echo '{"file":"m.gguf","done":1,"total":100}'
  exec "$S" 308"#;
    let (control, events) = control(sidecar(dir.path(), pull));
    control.pull().unwrap();
    wait_for(&events, is_progress);
    control.quit();
    assert!(gone(dir.path()), "the pull still runs");
    assert!(!control.state().pulling);
    // Nothing starts after Quit.
    assert!(control.pull().is_err());
    assert!(gone(dir.path()));
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
    assert_eq!(state.folder.as_deref(), dir.path().to_str());
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
    let files = [
        "m.gguf",
        "p.gguf",
        "l.safetensors",
        "d.onnx",
        "m.gguf.part",
        "p.gguf.part",
        "l.safetensors.part",
        "d.onnx.part",
    ];
    for name in files.iter().chain(&["other.txt", OLD_FILES[0]]) {
        fs::write(dir.path().join(name), "x").unwrap();
    }
    let state = control.remove().unwrap();
    assert!(!state.installed);
    assert_eq!(state.size_bytes, 0);
    for name in files {
        assert!(!dir.path().join(name).exists(), "{name} is still there");
    }
    assert!(dir.path().join("other.txt").exists());
    // Remove leaves the old model files to their own button.
    assert!(dir.path().join(OLD_FILES[0]).exists());
}

#[test]
fn a_sidecar_that_cannot_answer_is_a_problem() {
    let dir = tempfile::tempdir().unwrap();
    let (control, _) = control(spawn_program(dir.path().join("missing")));
    let state = control.state();
    assert!(!state.installed);
    let problem = state.problem.unwrap();
    assert!(
        problem.contains(": the notedown program could not start"),
        "{problem}"
    );
    let refused = control.pull().unwrap_err();
    assert!(
        refused.starts_with("The notedown program could not start"),
        "{refused}"
    );
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
    fs::write(dir.path().join("hold-path"), "").unwrap();
    fs::write(dir.path().join("m.gguf"), "x").unwrap();
    let remover = control.clone();
    let removing = std::thread::spawn(move || remover.remove());
    called(dir.path(), "model path");
    // The remove waits in `model path` until the release.
    assert_eq!(control.pull(), Err(REMOVE_RUNS.to_owned()));
    assert_eq!(control.remove().unwrap_err(), REMOVE_RUNS);
    fs::write(dir.path().join("release"), "").unwrap();
    let state = removing.join().unwrap().unwrap();
    assert!(!state.installed);
    assert!(!dir.path().join("m.gguf").exists());
}

#[test]
fn progress_after_a_pause_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    // The pull still says something while it ends.
    // The progress comes once the sleep runs: a SIGTERM to the group before its exec could
    // be lost in the forked shell and leave the sleep behind.
    let pull = r#"  trap 'echo "{\"file\":\"m.gguf\",\"done\":2,\"total\":100}"; exit 0' TERM
  "$S" 317 &
  child=$!
  i=0
  until [ "$(ps -o comm= -p "$child")" = "$S" ] || [ "$i" -ge 500 ]; do
    sleep 0.01
    i=$((i + 1))
  done
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
    assert!(gone(dir.path()), "{}", left_in(dir.path()));
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
    let ended = rx.recv_timeout(WAIT);
    // Never leave the fake behind, also when the check fails.
    kill_in(dir.path());
    let state = ended.expect("the check still runs after Quit");
    assert!(!state.installed);
    assert!(state.problem.is_some());
    // After Quit no check starts.
    assert!(control.state().problem.is_some());
}

#[test]
fn paths_needs_four_files() {
    let text = "cache: /c\nmodel: /c/m.gguf\nmmproj: /c/p.gguf\nlayout: /c/l.safetensors\nlines: /c/d.onnx\n";
    let paths = parse_paths(text).unwrap();
    assert_eq!(paths.cache, PathBuf::from("/c"));
    assert_eq!(
        paths.files,
        ["/c/m.gguf", "/c/p.gguf", "/c/l.safetensors", "/c/d.onnx"]
            .map(PathBuf::from)
            .to_vec()
    );
    // Every one of the five lines is needed: an old notedown names only two files.
    for key in ["cache: ", "model: ", "mmproj: ", "layout: ", "lines: "] {
        let short: String = text
            .lines()
            .filter(|l| !l.starts_with(key))
            .map(|l| format!("{l}\n"))
            .collect();
        assert!(parse_paths(&short).is_none(), "{key} is not needed");
    }
}

#[test]
fn all_lists_parts_of_four_files() {
    let paths = Paths {
        cache: PathBuf::from("/c"),
        files: ["m.gguf", "p.gguf", "l.safetensors", "d.onnx"]
            .map(|n| PathBuf::from("/c").join(n))
            .to_vec(),
    };
    let all: Vec<String> = paths
        .all()
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    assert_eq!(
        all,
        [
            "/c/m.gguf",
            "/c/p.gguf",
            "/c/l.safetensors",
            "/c/d.onnx",
            "/c/m.gguf.part",
            "/c/p.gguf.part",
            "/c/l.safetensors.part",
            "/c/d.onnx.part",
        ]
    );
}

#[test]
fn the_size_counts_all_four_files() {
    let dir = tempfile::tempdir().unwrap();
    let (control, _) = control(sidecar(dir.path(), "  exit 0"));
    fs::write(dir.path().join("m.gguf"), "12").unwrap();
    fs::write(dir.path().join("l.safetensors"), "123").unwrap();
    fs::write(dir.path().join("d.onnx.part"), "1234").unwrap();
    assert_eq!(control.state().size_bytes, 9);
}

#[test]
fn licences_come_from_the_pull_before_any_download() {
    let dir = tempfile::tempdir().unwrap();
    let (control, events) = control(sidecar(dir.path(), "  exit 1"));
    let file = |model: &str, page: &str, licensor: &str| LicenceFile {
        model: model.to_owned(),
        page: page.to_owned(),
        licence: "Apache-2.0".to_owned(),
        licensor: licensor.to_owned(),
    };
    let qwen = file(
        "Qwen3.5-4B",
        "https://huggingface.co/Qwen/Qwen3.5-4B",
        "Qwen team",
    );
    assert_eq!(
        control.licences().unwrap(),
        [
            qwen.clone(),
            qwen,
            file(
                "PP-DocLayoutV3",
                "https://huggingface.co/PaddlePaddle/PP-DocLayoutV3",
                "PaddlePaddle Authors"
            ),
        ]
    );
    // The pages of the models, once each, for open_link.
    assert_eq!(
        control.model_pages(),
        [
            "https://huggingface.co/Qwen/Qwen3.5-4B",
            "https://huggingface.co/PaddlePaddle/PP-DocLayoutV3",
        ]
    );
    let calls = fs::read_to_string(dir.path().join("calls")).unwrap();
    assert_eq!(calls.trim(), "model pull --json --licences-only");
    assert!(events.lock().unwrap().is_empty());
    assert!(!control.state().pulling);
}

#[test]
fn licences_without_files_are_an_error() {
    // A notedown program of 0.2.0 names lines only.
    assert_eq!(parse_licences(r#"{"licences":["A (B): C"]}"#), None);
    assert_eq!(parse_licences("nothing"), None);
}

#[test]
fn licences_without_a_licences_line_are_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let (control, _) = control(spawn_program(dir.path().join("missing")));
    let why = control.licences().unwrap_err();
    assert!(why.starts_with("The licences are not known"), "{why}");
}

#[test]
fn old_model_files_are_offered_and_clean_old_removes_them() {
    let dir = tempfile::tempdir().unwrap();
    let (control, _) = control(sidecar(dir.path(), "  exit 0"));
    assert!(!control.state().old_files);
    assert_eq!(control.state().old_bytes, 0);
    fs::write(dir.path().join(OLD_FILES[1]), "x").unwrap();
    assert!(control.state().old_files);
    let state = control.clean_old().unwrap();
    assert!(!state.old_files);
    assert_eq!(state.old_bytes, 0);
    assert!(!dir.path().join(OLD_FILES[1]).exists());
    let calls = fs::read_to_string(dir.path().join("calls")).unwrap();
    assert!(calls.lines().any(|l| l == "model clean-old"), "{calls}");
}

#[test]
fn the_old_size_counts_the_old_files_and_their_parts() {
    let dir = tempfile::tempdir().unwrap();
    let (control, _) = control(sidecar(dir.path(), "  exit 0"));
    fs::write(dir.path().join(OLD_FILES[0]), "12345").unwrap();
    fs::write(dir.path().join(format!("{}.part", OLD_FILES[1])), "12").unwrap();
    // The new model files do not count.
    fs::write(dir.path().join("m.gguf"), "123").unwrap();
    let state = control.state();
    assert!(state.old_files);
    assert_eq!(state.old_bytes, 7);
    // A part file that has no bytes yet is still offered for removal.
    fs::remove_file(dir.path().join(OLD_FILES[0])).unwrap();
    fs::write(dir.path().join(format!("{}.part", OLD_FILES[1])), "").unwrap();
    let state = control.state();
    assert!(state.old_files);
    assert_eq!(state.old_bytes, 0);
}

#[test]
fn the_state_names_the_folder_and_all_four_files() {
    let dir = tempfile::tempdir().unwrap();
    let (control, _) = control(sidecar(dir.path(), "  exit 0"));
    let state = control.state();
    assert_eq!(state.folder.as_deref(), dir.path().to_str());
    assert_eq!(state.files, ["m.gguf", "p.gguf", "l.safetensors", "d.onnx"]);
}
