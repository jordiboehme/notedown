//! With an app manifest, an app command may be called only from a window whose capability
//! grants its `allow-<command>` permission (capabilities/*.json). Without one, every window
//! could call every command.

/// Every command of `generate_handler!` in src/lib.rs. A test checks this list and the
/// capability files.
const COMMANDS: &[&str] = &[
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
    "log_in",
    "has_login",
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
    "library_state",
    "library_list",
    "library_entry",
    "library_recycle",
    "library_shown",
];

fn main() {
    let manifest = tauri_build::AppManifest::new().commands(COMMANDS);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))
        .expect("the Tauri build step failed");
}
