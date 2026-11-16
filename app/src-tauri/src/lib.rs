//! The Notedown menu bar app.

/// The bundle identifier of the app, as `tauri.conf.json` names it. The app's config, data and
/// Keychain entries are filed under it.
pub fn identifier() -> &'static str {
    "io.github.jordiboehme.notedown"
}

pub fn run() {
    tauri::Builder::default()
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
