//! The Notedown menu bar app.

pub mod commands;
pub mod config;
pub mod pidfile;
pub mod probe;
pub mod process;
pub mod processes;
pub mod serve_info;
pub mod supervisor;
pub mod token;

use std::sync::{Arc, mpsc};

use tauri::{AppHandle, Emitter, Manager, RunEvent};

use process::ChildKind;
use processes::{Options, Processes, REPROBE_EVERY};
use supervisor::ChildState;

/// The bundle identifier of the app, as `tauri.conf.json` names it. The app's config, data and
/// Keychain entries are filed under it.
pub fn identifier() -> &'static str {
    "io.github.jordiboehme.notedown"
}

/// The event that reports a new state of a child: `{ kind: "server" | "pipeline", state }`.
pub const PROCESS_STATE_EVENT: &str = "process-state";

#[derive(Clone, serde::Serialize)]
struct ProcessStateEvent {
    kind: &'static str,
    state: ChildState,
}

pub fn run() {
    let app = tauri::Builder::default()
        .setup(|app| {
            let config_dir = app.path().app_config_dir()?;
            app.manage(commands::ConfigState::load(config_dir));
            let sidecar = process::sidecar_path()?;
            let processes = Arc::new(Processes::new(Options {
                spawn: process::spawn_program(sidecar.clone()),
                clock: supervisor::system_clock(),
                sidecar,
                run_dir: app.path().app_data_dir()?.join("run"),
                parent: std::process::id(),
                server_grace: ChildKind::Server.grace(),
                pipeline_grace: ChildKind::Pipeline.grace(),
            }));
            report_states(app.handle().clone(), &processes);
            app.manage(processes);
            supervise(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::set_mode,
            commands::set_start_view,
            commands::start_processes,
            commands::stop_processes,
            commands::process_states
        ])
        .build(tauri::generate_context!())
        .expect("the Notedown app could not start");
    app.run(|app, event| {
        // Every way out ends here, also Cmd+Q and the Dock's Quit: no child stays behind. After
        // `quit_app` this finds nothing left to stop.
        if let RunEvent::Exit = event {
            stop_for_good(app);
        }
    });
}

/// Quit: stops every child off the main thread, then ends the app. The Quit item of the menu
/// and the panel call this.
pub fn quit_app(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        stop_for_good(&app);
        app.exit(0);
    });
}

/// Probes again what runs outside the app (when the panel opens), off the main thread: a probe
/// can wait for an answer.
pub fn reprobe_outside(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let config = app.state::<commands::ConfigState>().current();
        if let Some(processes) = app.try_state::<Arc<Processes>>() {
            processes.reprobe(&config);
        }
    });
}

/// Stops both children for good, at once. The model pull joins here later.
fn stop_for_good(app: &AppHandle) {
    if let Some(processes) = app.try_state::<Arc<Processes>>() {
        processes.quit();
    }
}

/// Starts the children as the config asks, then probes what is `Outside` every 30 s.
fn supervise(app: AppHandle) {
    std::thread::spawn(move || {
        let processes = app.state::<Arc<Processes>>().inner().clone();
        let config = || app.state::<commands::ConfigState>().current();
        processes.start_all(&config());
        loop {
            std::thread::sleep(REPROBE_EVERY);
            processes.reprobe(&config());
        }
    });
}

/// Sends every state change to the webview. The supervisors' listeners only put the change into
/// a channel, which never blocks; a thread of its own emits the events.
fn report_states(app: AppHandle, processes: &Processes) {
    let (tx, rx) = mpsc::channel::<ProcessStateEvent>();
    for sup in [processes.server(), processes.pipeline()] {
        let tx = tx.clone();
        let kind = sup.kind().name();
        sup.on_change(move |state| {
            let _ = tx.send(ProcessStateEvent {
                kind,
                state: state.clone(),
            });
        });
    }
    std::thread::spawn(move || {
        for event in rx {
            let _ = app.emit(PROCESS_STATE_EVENT, event);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_identifier_is_notedowns() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(config["identifier"], identifier());
        assert_eq!(identifier(), "io.github.jordiboehme.notedown");
    }
}
