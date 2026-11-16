//! The commands the webview calls for the config and the children.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::Serialize;
use tauri::{AppHandle, State};

use crate::config::{self, AppConfig, Mode, StartView};
use crate::model::{ModelControl, ModelState};
use crate::poller::Poller;
use crate::processes::{ProcessStates, Processes};
use crate::status::Status;
use crate::token::{KeychainStore, TokenStore, token_key};
use crate::windows::{self, WindowKind};

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

    /// Sets where the server runs. Remote needs an `http://` or `https://` address; a folder
    /// must be a full path (a leading `~` is the home folder). A new server forgets the account
    /// and the e-mail, which belong to the old one: another mode, another address, or in Local
    /// mode another folder (there the folder is the server).
    pub fn set_mode(
        &self,
        mode: Mode,
        data_dir: Option<PathBuf>,
        server_url: Option<String>,
    ) -> Result<AppConfig, String> {
        let server_url = server_url
            .map(|u| u.trim().to_owned())
            .filter(|u| !u.is_empty());
        if mode == Mode::Remote
            && !server_url
                .as_deref()
                .is_some_and(|u| u.starts_with("http://") || u.starts_with("https://"))
        {
            return Err(NEEDS_ADDRESS.to_owned());
        }
        let data_dir = data_dir.map(|d| expand_home(&d)).transpose()?;
        self.update(|c| {
            let before = (c.mode, c.server_url.clone(), c.data_dir.clone());
            c.mode = mode;
            if let Some(dir) = data_dir {
                c.data_dir = dir;
            }
            c.server_url = server_url;
            let new_server = before.0 != c.mode
                || before.1 != c.server_url
                || (c.mode == Mode::Local && before.2 != c.data_dir);
            if new_server {
                c.account = None;
                c.email = None;
            }
        })
    }

    pub fn set_start_view(&self, view: StartView) -> Result<AppConfig, String> {
        self.update(|c| c.start_view = view)
    }
}

/// What Set refuses for Another machine without a web address.
pub const NEEDS_ADDRESS: &str =
    "Another machine needs its address, starting with http:// or https://.";
/// What Set refuses for a folder that is not a full path.
pub const NEEDS_FULL_PATH: &str = "The folder must be a full path, such as ~/notedown-data.";

/// The two links of the About page; nothing else opens.
pub const ABOUT_LINKS: [&str; 2] = [
    "https://ko-fi.com/V7V31T6CL9",
    "https://github.com/jordiboehme/notedown",
];

/// How many lines of each child's log the Server page shows.
pub const LOG_SHOWN: usize = 30;

/// `~` and `~/...` are under the home folder; the result must be absolute.
fn expand_home(dir: &Path) -> Result<PathBuf, String> {
    let text = dir.to_string_lossy();
    let text = text.trim();
    let expanded = if text == "~" || text.starts_with("~/") {
        let home = std::env::var_os("HOME").ok_or_else(|| NEEDS_FULL_PATH.to_owned())?;
        PathBuf::from(home).join(text.trim_start_matches('~').trim_start_matches('/'))
    } else {
        PathBuf::from(text)
    };
    if expanded.is_absolute() {
        // `home.join("")` leaves a trailing slash; components drop it.
        Ok(expanded.components().collect())
    } else {
        Err(NEEDS_FULL_PATH.to_owned())
    }
}

/// Log out: deletes the token of the account in the config. The account stays, so the
/// pipeline still knows whose notes it reads.
pub fn log_out_of(store: &dyn TokenStore, config: &AppConfig) -> Result<(), String> {
    match token_key(config) {
        Some(key) => store.delete(&key).map_err(|e| e.to_string()),
        None => Ok(()),
    }
}

pub fn allowed_link(url: &str) -> bool {
    ABOUT_LINKS.contains(&url)
}

pub fn last_lines(mut lines: Vec<String>, n: usize) -> Vec<String> {
    let skip = lines.len().saturating_sub(n);
    lines.drain(..skip);
    lines
}

#[tauri::command]
pub fn get_config(state: State<'_, ConfigState>) -> Result<AppConfig, String> {
    state
        .config
        .lock()
        .map(|c| c.clone())
        .map_err(|e| e.to_string())
}

/// Sets where the server runs (the mode control on the Server page). When the mode, the folder
/// or the address changed, both children start afresh for it, off the main thread: the start
/// first stops what runs.
#[tauri::command]
pub async fn set_mode(
    state: State<'_, ConfigState>,
    processes: State<'_, Arc<Processes>>,
    mode: Mode,
    data_dir: Option<PathBuf>,
    server_url: Option<String>,
) -> Result<AppConfig, String> {
    let before = state.current();
    let config = state.set_mode(mode, data_dir, server_url)?;
    let changed = (before.mode, &before.data_dir, &before.server_url)
        != (config.mode, &config.data_dir, &config.server_url);
    if changed {
        let processes = processes.inner().clone();
        let next = config.clone();
        tauri::async_runtime::spawn_blocking(move || processes.start_all(&next))
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(config)
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

/// Opens the library window. Async, so the window is made off the main thread.
#[tauri::command]
pub async fn open_library(app: AppHandle) -> Result<(), String> {
    windows::show_window(&app, WindowKind::Library, None).map_err(|e| e.to_string())
}

/// Opens the settings window, at `page` when given.
#[tauri::command]
pub async fn open_settings(app: AppHandle, page: Option<String>) -> Result<(), String> {
    windows::show_window(&app, WindowKind::Settings, page).map_err(|e| e.to_string())
}

/// Quit: stops every child, then ends the app. Returns at once.
#[tauri::command]
pub fn quit_app(app: AppHandle) {
    crate::quit_app(&app);
}

/// Quit, but ask first in the panel while the Supernote needs the server.
#[tauri::command]
pub fn request_quit(app: AppHandle) {
    crate::request_quit(&app);
}

/// Log out: deletes the token from the Keychain, off the main thread, then polls again.
#[tauri::command]
pub async fn log_out(
    config: State<'_, ConfigState>,
    poller: State<'_, Poller>,
) -> Result<(), String> {
    let config = config.current();
    tauri::async_runtime::spawn_blocking(move || log_out_of(&KeychainStore::new(), &config))
        .await
        .map_err(|e| e.to_string())??;
    poller.poll_now();
    Ok(())
}

/// The last lines of both children's output, for the Server page.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessLogs {
    pub server: Vec<String>,
    pub pipeline: Vec<String>,
}

#[tauri::command]
pub fn process_logs(processes: State<'_, Arc<Processes>>) -> ProcessLogs {
    ProcessLogs {
        server: last_lines(processes.server().log(), LOG_SHOWN),
        pipeline: last_lines(processes.pipeline().log(), LOG_SHOWN),
    }
}

/// What the MCP config of an agent names: the sidecar and the data folder.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpInfo {
    pub sidecar: String,
    pub data_dir: String,
}

#[tauri::command]
pub fn mcp_info(config: State<'_, ConfigState>) -> Result<McpInfo, String> {
    let sidecar = crate::process::sidecar_path().map_err(|e| e.to_string())?;
    Ok(McpInfo {
        sidecar: sidecar.display().to_string(),
        data_dir: config.current().data_dir.display().to_string(),
    })
}

/// Opens one of the About links in the browser. No other address opens.
#[tauri::command]
pub async fn open_link(url: String) -> Result<(), String> {
    if !allowed_link(&url) {
        return Err("This link does not open.".to_owned());
    }
    tauri::async_runtime::spawn_blocking(move || {
        std::process::Command::new("/usr/bin/open")
            .arg(&url)
            .status()
            .map_err(|e| e.to_string())
            .and_then(|status| {
                if status.success() {
                    Ok(())
                } else {
                    Err("The browser did not open.".to_owned())
                }
            })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Whether Notedown starts at login (a LaunchAgent).
#[tauri::command]
pub async fn get_autostart(app: AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

/// Turns the start at login on or off; returns what is set now.
#[tauri::command]
pub async fn set_autostart(app: AppHandle, on: bool) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    if on {
        manager.enable()
    } else {
        manager.disable()
    }
    .map_err(|e| e.to_string())?;
    manager.is_enabled().map_err(|e| e.to_string())
}

/// The OCR model as it is now. Off the main thread: `model verify` reads every byte.
#[tauri::command]
pub async fn model_state(model: State<'_, ModelControl>) -> Result<ModelState, String> {
    let model = model.inner().clone();
    tauri::async_runtime::spawn_blocking(move || model.state())
        .await
        .map_err(|e| e.to_string())
}

/// Starts the model download; the progress comes as events. Refused while one runs. Async, so
/// the spawn does not run on the main thread.
#[tauri::command]
pub async fn model_pull(model: State<'_, ModelControl>) -> Result<(), String> {
    model.pull()
}

/// Stops the model download (Download continues it), then says what is on disk.
#[tauri::command]
pub async fn model_pause(model: State<'_, ModelControl>) -> Result<ModelState, String> {
    let model = model.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        model.pause();
        model.state()
    })
    .await
    .map_err(|e| e.to_string())
}

/// Deletes the model files. Refused while a download runs.
#[tauri::command]
pub async fn model_remove(model: State<'_, ModelControl>) -> Result<ModelState, String> {
    let model = model.inner().clone();
    tauri::async_runtime::spawn_blocking(move || model.remove())
        .await
        .map_err(|e| e.to_string())?
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
    fn set_mode_checks_its_input() {
        let dir = tempfile::tempdir().unwrap();
        let state = ConfigState::load(dir.path().to_owned());
        // Another machine needs its address.
        for url in [None, Some("  "), Some("pi.local:19072")] {
            let err = state
                .set_mode(Mode::Remote, None, url.map(str::to_owned))
                .unwrap_err();
            assert_eq!(err, NEEDS_ADDRESS);
        }
        // A folder must be a full path; a leading ~ is the home folder.
        let err = state
            .set_mode(Mode::Local, Some("notes".into()), None)
            .unwrap_err();
        assert_eq!(err, NEEDS_FULL_PATH);
        assert_eq!(state.current().mode, Mode::NotSet);
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let cfg = state
            .set_mode(Mode::Local, Some("~/notes".into()), None)
            .unwrap();
        assert_eq!(cfg.data_dir, home.join("notes"));
        let cfg = state.set_mode(Mode::Local, Some("~".into()), None).unwrap();
        assert_eq!(cfg.data_dir, home);
        let cfg = state
            .set_mode(Mode::Remote, None, Some(" https://n.example.org ".into()))
            .unwrap();
        assert_eq!(cfg.server_url.as_deref(), Some("https://n.example.org"));
    }

    #[test]
    fn a_new_server_forgets_the_account() {
        let dir = tempfile::tempdir().unwrap();
        let state = ConfigState::load(dir.path().to_owned());
        let login = |state: &ConfigState| {
            state
                .update(|c| {
                    c.account = Some("jordi".into());
                    c.email = Some("jordi@example.org".into());
                })
                .unwrap();
        };
        state
            .set_mode(Mode::Remote, None, Some("https://a.example.org".into()))
            .unwrap();
        login(&state);
        // The same server again keeps the account.
        let cfg = state
            .set_mode(Mode::Remote, None, Some("https://a.example.org".into()))
            .unwrap();
        assert_eq!(cfg.account.as_deref(), Some("jordi"));
        // Another folder in Remote mode is the same server.
        let cfg = state
            .set_mode(
                Mode::Remote,
                Some("/data/other".into()),
                Some("https://a.example.org".into()),
            )
            .unwrap();
        assert_eq!(cfg.account.as_deref(), Some("jordi"));
        // Another server forgets it.
        let cfg = state
            .set_mode(Mode::Remote, None, Some("https://b.example.org".into()))
            .unwrap();
        assert_eq!((cfg.account, cfg.email), (None, None));
        login(&state);
        // Another mode forgets it.
        let cfg = state
            .set_mode(Mode::Local, Some("/data/x".into()), None)
            .unwrap();
        assert_eq!((cfg.account, cfg.email), (None, None));
        login(&state);
        // In Local mode the folder is the server.
        let cfg = state
            .set_mode(Mode::Local, Some("/data/x".into()), None)
            .unwrap();
        assert_eq!(cfg.account.as_deref(), Some("jordi"));
        let cfg = state
            .set_mode(Mode::Local, Some("/data/y".into()), None)
            .unwrap();
        assert_eq!((cfg.account, cfg.email), (None, None));
    }

    #[test]
    fn log_out_deletes_the_token_only() {
        use crate::token::{MemoryStore, Token, TokenStore};
        let store = MemoryStore::new();
        let config = AppConfig {
            mode: Mode::Local,
            account: Some("jordi".into()),
            ..AppConfig::default()
        };
        store.set("local|jordi", &Token::new("secret")).unwrap();
        store.set("local|other", &Token::new("other")).unwrap();
        log_out_of(&store, &config).unwrap();
        assert_eq!(store.get("local|jordi").unwrap(), None);
        assert!(store.get("local|other").unwrap().is_some());
        // Without an account there is nothing to delete.
        log_out_of(&store, &AppConfig::default()).unwrap();
    }

    #[test]
    fn only_the_about_links_open() {
        assert!(allowed_link("https://ko-fi.com/V7V31T6CL9"));
        assert!(allowed_link("https://github.com/jordiboehme/notedown"));
        assert!(!allowed_link("https://example.org"));
        assert!(!allowed_link("file:///etc/passwd"));
    }

    #[test]
    fn the_log_shows_its_last_lines() {
        let lines: Vec<String> = (0..50).map(|i| i.to_string()).collect();
        let last = last_lines(lines, 3);
        assert_eq!(last, vec!["47", "48", "49"]);
        assert_eq!(last_lines(vec!["a".into()], 3), vec!["a"]);
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
