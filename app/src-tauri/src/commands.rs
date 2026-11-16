//! The commands the webview calls for the config.

use std::path::PathBuf;
use std::sync::Mutex;

use tauri::State;

use crate::config::{self, AppConfig, Mode, StartView};

/// The live config and the folder it is saved in.
pub struct ConfigState {
    pub dir: PathBuf,
    pub config: Mutex<AppConfig>,
}

impl ConfigState {
    pub fn load(dir: PathBuf) -> Self {
        let config = Mutex::new(config::load(&dir));
        Self { dir, config }
    }

    /// Changes the config and saves it; the change stays in memory only when the save worked.
    fn update(&self, change: impl FnOnce(&mut AppConfig)) -> Result<AppConfig, String> {
        let mut guard = self.config.lock().map_err(|e| e.to_string())?;
        let mut next = guard.clone();
        change(&mut next);
        config::save(&self.dir, &next).map_err(|e| format!("the config was not saved: {e}"))?;
        *guard = next.clone();
        Ok(next)
    }

    pub fn set_mode(
        &self,
        mode: Mode,
        data_dir: Option<PathBuf>,
        server_url: Option<String>,
    ) -> Result<AppConfig, String> {
        self.update(|c| {
            c.mode = mode;
            if let Some(dir) = data_dir {
                c.data_dir = dir;
            }
            c.server_url = server_url.filter(|u| !u.trim().is_empty());
        })
    }

    pub fn set_start_view(&self, view: StartView) -> Result<AppConfig, String> {
        self.update(|c| c.start_view = view)
    }
}

#[tauri::command]
pub fn get_config(state: State<'_, ConfigState>) -> Result<AppConfig, String> {
    state
        .config
        .lock()
        .map(|c| c.clone())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn set_mode(
    state: State<'_, ConfigState>,
    mode: Mode,
    data_dir: Option<PathBuf>,
    server_url: Option<String>,
) -> Result<AppConfig, String> {
    state.set_mode(mode, data_dir, server_url)
}

#[tauri::command]
pub fn set_start_view(
    state: State<'_, ConfigState>,
    start_view: StartView,
) -> Result<AppConfig, String> {
    state.set_start_view(start_view)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_mode_and_start_view_are_saved() {
        let dir = tempfile::tempdir().unwrap();
        let state = ConfigState::load(dir.path().to_owned());
        let cfg = state
            .set_mode(Mode::Local, Some("/data/x".into()), Some("  ".into()))
            .unwrap();
        assert_eq!(cfg.mode, Mode::Local);
        assert_eq!(cfg.data_dir, PathBuf::from("/data/x"));
        assert_eq!(cfg.server_url, None);
        state.set_start_view(StartView::Columns).unwrap();
        let reloaded = config::load(dir.path());
        assert_eq!(reloaded.mode, Mode::Local);
        assert_eq!(reloaded.start_view, StartView::Columns);
    }

    #[test]
    fn set_mode_without_a_folder_keeps_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let state = ConfigState::load(dir.path().to_owned());
        state
            .set_mode(Mode::Local, Some("/data/x".into()), None)
            .unwrap();
        let cfg = state
            .set_mode(Mode::Remote, None, Some("https://n.example.org".into()))
            .unwrap();
        assert_eq!(cfg.data_dir, PathBuf::from("/data/x"));
        assert_eq!(cfg.server_url.as_deref(), Some("https://n.example.org"));
    }
}
