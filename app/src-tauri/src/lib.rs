//! The Notedown menu bar app.

pub mod commands;
pub mod config;
pub mod pidfile;
pub mod poller;
pub mod probe;
pub mod process;
pub mod processes;
pub mod quit;
pub mod serve_info;
pub mod status;
pub mod supervisor;
pub mod token;
pub mod tray;
pub mod windows;

use std::sync::{Arc, mpsc};

use tauri::{AppHandle, Emitter, Manager, RunEvent};

use poller::{Inputs, Poller, RealSources, TICK_EVERY};
use process::ChildKind;
use processes::{Options, Processes, REPROBE_EVERY};
use quit::{ExitDecision, QuitFlow};
use status::Status;
use supervisor::ChildState;
use token::KeychainStore;
use tray::{TrayIconKind, tray_icon_for};
use windows::Shown;

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
        // First: a second launch hands over to the running app and ends inside this plugin,
        // before `setup` could reclaim the running app's children as its own.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            windows::show_panel(app);
        }))
        .menu(tray::app_menu)
        .on_menu_event(|app, event| tray::on_menu_event(app, event.id().as_ref()))
        .on_window_event(windows::on_window_event)
        .manage(Shown::default())
        .manage(Arc::new(QuitFlow::new()))
        .setup(|app| {
            // A menu bar app: no Dock icon until a library or settings window shows. The
            // Info.plist says so too (LSUIElement), so the bundle never shows one at launch.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            tray::build(app.handle())?;
            app.manage(windows::PollerVisibility::start(app.handle()));
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
            commands::get_status,
            commands::open_library,
            commands::open_settings,
            commands::quit_app
        ])
        .build(tauri::generate_context!())
        .expect("the Notedown app could not start");
    app.run(|app, event| match event {
        // Any exit request waits for the Quit flow; closing the last window (no code) never
        // quits a menu bar app.
        RunEvent::ExitRequested { code, api, .. } => {
            match app.state::<Arc<QuitFlow>>().on_exit_request(code) {
                ExitDecision::Allow => {}
                ExitDecision::Prevent => api.prevent_exit(),
                ExitDecision::PreventAndQuit => {
                    api.prevent_exit();
                    quit_app(app);
                }
            }
        }
        // The last stop for a way out that skips the Quit flow (the Dock's native Quit,
        // logout). After the Quit flow it finds nothing left to stop.
        RunEvent::Exit => stop_for_good(app),
        _ => {}
    });
}

/// Quit: hides the windows, stops every child off the main thread, then ends the app. The
/// Quit items, Cmd+Q, the panel and every exit request run this; a second call while it runs
/// does nothing.
pub fn quit_app(app: &AppHandle) {
    let flow = app.state::<Arc<QuitFlow>>().inner().clone();
    let stop_app = app.clone();
    let exit_app = app.clone();
    windows::hide_all(app);
    flow.quit_with(move || stop_for_good(&stop_app), move || exit_app.exit(0));
}

/// Probes again what runs outside the app (when the panel opens), off the main thread: a probe
/// can wait for an answer.
pub fn reprobe_outside(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        // A second launch can open the panel before `setup` has managed these.
        if let (Some(config), Some(processes)) = (
            app.try_state::<commands::ConfigState>(),
            app.try_state::<Arc<Processes>>(),
        ) {
            processes.reprobe(&config.current());
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
        let mut icon = TrayIconKind::Normal;
        for status in rx {
            let kind = tray_icon_for(&status.attention);
            if kind != icon {
                tray::set_icon(&app, kind);
                icon = kind;
            }
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
