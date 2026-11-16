//! The Notedown menu bar app.

pub mod commands;
pub mod config;
pub mod model;
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

use model::{ModelControl, ModelEvent};
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

/// How long a stopped model download may take to end before SIGKILL. A download has nothing
/// to save: its part file is kept as it is.
const MODEL_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

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
        // "Start at login" on the General page. Only Rust calls it (get_autostart,
        // set_autostart); no window has its permissions. It is off until the user turns it on.
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .macos_launcher(tauri_plugin_autostart::MacosLauncher::LaunchAgent)
                .build(),
        )
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
            app.manage(model_control(app.handle().clone(), sidecar_spawn.clone()));
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
            commands::quit_app,
            commands::request_quit,
            commands::log_out,
            commands::process_logs,
            commands::mcp_info,
            commands::open_link,
            commands::get_autostart,
            commands::set_autostart,
            commands::model_state,
            commands::model_pull,
            commands::model_pause,
            commands::model_remove
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

/// Quit: hides the windows, stops every child off the main thread, then ends the app.
/// [`request_quit`], the Quit of the panel's question and every exit request run this; a
/// second call while it runs does nothing.
pub fn quit_app(app: &AppHandle) {
    let flow = app.state::<Arc<QuitFlow>>().inner().clone();
    let stop_app = app.clone();
    let exit_app = app.clone();
    windows::hide_all(app);
    flow.quit_with(move || stop_for_good(&stop_app), move || exit_app.exit(0));
}

/// Quit as the user asks for it (the Quit items, Cmd+Q, the panel's Quit): while the app's own
/// server runs in Local mode, the panel opens and asks first; otherwise the app quits at once.
/// The decision reads the supervisors, so it runs off the main thread.
pub fn request_quit(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let ask = match (
            app.try_state::<commands::ConfigState>(),
            app.try_state::<Arc<Processes>>(),
        ) {
            (Some(config), Some(processes)) => {
                quit::quit_needs_question(config.current().mode, &processes.states().server)
            }
            _ => false,
        };
        if ask {
            if !app.state::<Shown>().is_visible(windows::WindowKind::Panel) {
                windows::show_panel(&app);
            }
            let _ = app.emit_to(
                windows::WindowKind::Panel.label(),
                quit::CONFIRM_QUIT_EVENT,
                (),
            );
        } else {
            quit_app(&app);
        }
    });
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
            let config = config.reader();
            processes.reprobe_with(|| config.current());
        }
    });
}

/// Stops both children and the model download for good, all at once.
fn stop_for_good(app: &AppHandle) {
    if let Some(poller) = app.try_state::<Poller>() {
        poller.stop();
    }
    let model = app.try_state::<ModelControl>().map(|m| m.inner().clone());
    std::thread::scope(|s| {
        if let Some(model) = model {
            s.spawn(move || model.quit());
        }
        if let Some(processes) = app.try_state::<Arc<Processes>>() {
            processes.quit();
        }
    });
}

/// The model download. Its events only go into a channel; a thread of its own emits them.
fn model_control(app: AppHandle, spawn: process::SpawnFn) -> ModelControl {
    let (tx, rx) = mpsc::channel::<ModelEvent>();
    std::thread::spawn(move || {
        for event in rx {
            let _ = match event {
                ModelEvent::Progress(progress) => app.emit(model::MODEL_PROGRESS_EVENT, progress),
                ModelEvent::Ready(state) => app.emit(model::MODEL_READY_EVENT, state),
                ModelEvent::Failed { text } => app.emit(
                    model::MODEL_FAILED_EVENT,
                    serde_json::json!({ "text": text }),
                ),
            };
        }
    });
    ModelControl::new(
        spawn,
        MODEL_GRACE,
        Arc::new(move |event: &ModelEvent| {
            let _ = tx.send(event.clone());
        }),
    )
}

/// Starts the children as the config asks, then probes what is `Outside` every 30 s.
fn supervise(app: AppHandle) {
    std::thread::spawn(move || {
        let processes = app.state::<Arc<Processes>>().inner().clone();
        let config = app.state::<commands::ConfigState>().reader();
        processes.start_with(|| config.current());
        loop {
            std::thread::sleep(REPROBE_EVERY);
            processes.reprobe_with(|| config.current());
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
    use std::collections::BTreeSet;

    /// The app commands a window may call, from all capability files that name it.
    fn granted(window: &str) -> BTreeSet<String> {
        let files = [
            include_str!("../capabilities/default.json"),
            include_str!("../capabilities/panel.json"),
            include_str!("../capabilities/settings.json"),
        ];
        let mut commands = BTreeSet::new();
        for text in files {
            let cap: serde_json::Value = serde_json::from_str(text).unwrap();
            let windows = cap["windows"].as_array().unwrap();
            if !windows.iter().any(|w| w == window) {
                continue;
            }
            for permission in cap["permissions"].as_array().unwrap() {
                let name = permission.as_str().unwrap();
                // App permissions have no plugin prefix: `allow-get-status`.
                if let Some(command) = name.strip_prefix("allow-") {
                    commands.insert(command.replace('-', "_"));
                }
            }
        }
        commands
    }

    /// The commands build.rs gives permissions to, in its `COMMANDS` list.
    fn manifest_commands() -> BTreeSet<String> {
        let build = include_str!("../build.rs");
        let start = build
            .find("const COMMANDS")
            .expect("no COMMANDS in build.rs");
        let list = &build[start..build[start..].find("];").unwrap() + start];
        list.split('"')
            .skip(1)
            .step_by(2)
            .map(str::to_owned)
            .collect()
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    const SETTINGS_ONLY: [&str; 16] = [
        "get_config",
        "set_mode",
        "set_start_view",
        "start_processes",
        "stop_processes",
        "set_autostart",
        "get_autostart",
        "log_out",
        "model_pull",
        "model_pause",
        "model_remove",
        "model_state",
        "mcp_info",
        "process_logs",
        "open_link",
        "process_states",
    ];

    #[test]
    fn each_window_may_call_only_its_commands() {
        assert_eq!(
            granted("panel"),
            set(&[
                "get_status",
                "open_library",
                "open_settings",
                "request_quit",
                "quit_app"
            ])
        );
        assert_eq!(granted("library"), set(&[]));
        let mut settings = set(&SETTINGS_ONLY);
        settings.remove("process_states");
        settings.insert("get_status".into());
        assert_eq!(granted("settings"), settings);
        for command in SETTINGS_ONLY {
            assert!(!granted("panel").contains(command), "panel has {command}");
            assert!(
                !granted("library").contains(command),
                "library has {command}"
            );
        }
    }

    #[test]
    fn every_app_command_is_in_the_manifest() {
        let all = set(&[
            "get_config",
            "set_mode",
            "set_start_view",
            "start_processes",
            "stop_processes",
            "process_states",
            "get_status",
            "open_library",
            "open_settings",
            "quit_app",
            "request_quit",
            "log_out",
            "process_logs",
            "mcp_info",
            "open_link",
            "get_autostart",
            "set_autostart",
            "model_state",
            "model_pull",
            "model_pause",
            "model_remove",
        ]);
        assert_eq!(manifest_commands(), all);
        // Every granted command is a real one.
        for window in ["panel", "library", "settings"] {
            assert!(granted(window).is_subset(&all), "{window}");
        }
    }

    #[test]
    fn the_identifier_is_notedowns() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(config["identifier"], identifier());
        assert_eq!(identifier(), "io.github.jordiboehme.notedown");
    }
}
