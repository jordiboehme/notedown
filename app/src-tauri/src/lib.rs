//! The Notedown menu bar app.

pub mod commands;
pub mod config;
pub mod pidfile;
pub mod poller;
pub mod probe;
pub mod process;
pub mod processes;
pub mod serve_info;
pub mod status;
pub mod supervisor;
pub mod token;

use std::sync::{Arc, mpsc};

use tauri::{AppHandle, Emitter, Manager, RunEvent};

use poller::{Inputs, Poller, RealSources, TICK_EVERY};
use process::ChildKind;
use processes::{Options, Processes, REPROBE_EVERY};
use status::Status;
use supervisor::ChildState;
use token::KeychainStore;

/// The bundle identifier of the app, as `tauri.conf.json` names it. The app's config, data and
/// Keychain entries are filed under it.
pub fn identifier() -> &'static str {
    "io.github.jordiboehme.notedown"
}

/// The event that reports a new state of a child: `{ kind: "server" | "pipeline", state }`.
pub const PROCESS_STATE_EVENT: &str = "process-state";

/// The event that carries a new [`Status`].
pub const STATUS_EVENT: &str = "status";

#[derive(Clone, serde::Serialize)]
struct ProcessStateEvent {
    kind: &'static str,
    state: ChildState,
}

pub fn run() {
    let app = tauri::Builder::default()
        .setup(|app| {
            let config_dir = app.path().app_config_dir()?;
            let config = commands::ConfigState::load(config_dir);
            let reader = config.reader();
            app.manage(config);
            let sidecar = process::sidecar_path()?;
            let sidecar_spawn = process::spawn_program(sidecar.clone());
            let processes = Arc::new(Processes::new(Options {
                spawn: sidecar_spawn.clone(),
                clock: supervisor::system_clock(),
                sidecar,
                run_dir: app.path().app_data_dir()?.join("run"),
                parent: std::process::id(),
                server_grace: ChildKind::Server.grace(),
                pipeline_grace: ChildKind::Pipeline.grace(),
            }));
            let poller = status_poller(sidecar_spawn, reader, processes.clone());
            report_states(app.handle().clone(), &processes, poller.clone());
            report_status(app.handle().clone(), &poller);
            app.manage(processes);
            app.manage(poller.clone());
            supervise(app.handle().clone());
            poll(poller);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::set_mode,
            commands::set_start_view,
            commands::start_processes,
            commands::stop_processes,
            commands::process_states,
            commands::get_status
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
    if let Some(poller) = app.try_state::<Poller>() {
        poller.stop();
    }
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

/// The status poller. It reads the config and the children through handles of their own, so
/// it holds no handle to the app.
fn status_poller(
    spawn: process::SpawnFn,
    config: commands::ConfigReader,
    processes: Arc<Processes>,
) -> Poller {
    Poller::new(
        Arc::new(RealSources::new(spawn)),
        Arc::new(KeychainStore::new()),
        Arc::new(move || Inputs {
            config: config.current(),
            config_reset: config.was_reset(),
            processes: processes.states(),
            serve_info: processes.serve_info(),
        }),
        supervisor::system_clock(),
    )
}

/// Ticks the poller once a second: it starts the polls that are due and makes the status
/// again (the dot for a server that does not answer depends on the time).
fn poll(poller: Poller) {
    std::thread::spawn(move || {
        loop {
            poller.tick();
            std::thread::sleep(TICK_EVERY);
        }
    });
}

/// Sends every new status to the webview. The poller's listener only puts it into a channel;
/// a thread of its own emits the events.
fn report_status(app: AppHandle, poller: &Poller) {
    let (tx, rx) = mpsc::channel::<Status>();
    poller.on_status(move |status| {
        let _ = tx.send(status.clone());
    });
    std::thread::spawn(move || {
        for status in rx {
            let _ = app.emit(STATUS_EVENT, status);
        }
    });
}

/// Sends every state change to the webview and makes the status again. The supervisors'
/// listeners only put the change into a channel, which never blocks; a thread of its own emits
/// the events.
fn report_states(app: AppHandle, processes: &Processes, poller: Poller) {
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
            poller.refresh();
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
