//! The Notedown menu bar app.

pub mod commands;
pub mod config;
pub mod process;
pub mod supervisor;
pub mod token;

/// The bundle identifier of the app, as `tauri.conf.json` names it. The app's config, data and
/// Keychain entries are filed under it.
pub fn identifier() -> &'static str {
    "io.github.jordiboehme.notedown"
}

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            use tauri::Manager;
            let dir = app.path().app_config_dir()?;
            app.manage(commands::ConfigState::load(dir));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::set_mode,
            commands::set_start_view
        ])
        .run(tauri::generate_context!())
        .expect("the Notedown app could not start");
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
