//! The commands the webview calls for the config and the children.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tauri::State;

use crate::config::{self, AppConfig, Mode, StartView};
use crate::poller::Poller;
use crate::processes::{ProcessStates, Processes};
use crate::status::Status;

/// The live config and the folder it is saved in.
pub struct ConfigState {
    pub dir: PathBuf,
    pub config: Arc<Mutex<AppConfig>>,
    /// The file was broken and set aside at start; cleared by the next save.
    reset: Arc<AtomicBool>,
}

/// Reads the live config from another thread (the poller), without a handle to the app.
#[derive(Clone)]
pub struct ConfigReader {
    config: Arc<Mutex<AppConfig>>,
    reset: Arc<AtomicBool>,
}

impl ConfigReader {
    pub fn current(&self) -> AppConfig {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn was_reset(&self) -> bool {
        self.reset.load(Ordering::SeqCst)
    }
}

impl ConfigState {
    pub fn load(dir: PathBuf) -> Self {
        let loaded = config::load_checked(&dir);
        Self {
            dir,
            config: Arc::new(Mutex::new(loaded.config)),
            reset: Arc::new(AtomicBool::new(loaded.reset)),
        }
    }

    pub fn reader(&self) -> ConfigReader {
        ConfigReader {
            config: self.config.clone(),
            reset: self.reset.clone(),
        }
    }

    /// The config was reset at start and has not been saved since.
    pub fn was_reset(&self) -> bool {
        self.reset.load(Ordering::SeqCst)
    }

    /// The config as it is now.
    pub fn current(&self) -> AppConfig {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Changes the config and saves it; the change stays in memory only when the save worked.
    fn update(&self, change: impl FnOnce(&mut AppConfig)) -> Result<AppConfig, String> {
        let mut guard = self.config.lock().map_err(|e| e.to_string())?;
        let mut next = guard.clone();
        change(&mut next);
        config::save(&self.dir, &next).map_err(|e| format!("the config was not saved: {e}"))?;
        *guard = next.clone();
        self.reset.store(false, Ordering::SeqCst);
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

/// Starts both children afresh as the config asks (Start on the Server page). Off the main
/// thread: a start first stops what runs, which can take the grace.
#[tauri::command]
pub async fn start_processes(
    config: State<'_, ConfigState>,
    processes: State<'_, Arc<Processes>>,
) -> Result<ProcessStates, String> {
    let config = config.current();
    let processes = processes.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        processes.start_all(&config);
        processes.states()
    })
    .await
    .map_err(|e| e.to_string())
}

/// Stops both children (Stop on the Server page), off the main thread.
#[tauri::command]
pub async fn stop_processes(processes: State<'_, Arc<Processes>>) -> Result<ProcessStates, String> {
    let processes = processes.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        processes.stop_all();
        processes.states()
    })
    .await
    .map_err(|e| e.to_string())
}

/// The status as it is now, from the last answers of the poller. It waits for no call.
#[tauri::command]
pub fn get_status(poller: State<'_, Poller>) -> Status {
    poller.status()
}

#[tauri::command]
pub fn process_states(processes: State<'_, Arc<Processes>>) -> ProcessStates {
    processes.states()
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
    fn a_broken_config_is_reported_until_the_next_save() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), "{ not json").unwrap();
        let state = ConfigState::load(dir.path().to_owned());
        assert!(state.was_reset());
        assert_eq!(state.current().mode, Mode::NotSet);
        let reader = state.reader();
        assert!(reader.was_reset());
        state.set_start_view(StartView::Columns).unwrap();
        assert!(!state.was_reset());
        assert!(!reader.was_reset());
        assert_eq!(reader.current().start_view, StartView::Columns);
        assert!(!ConfigState::load(dir.path().to_owned()).was_reset());
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
