//! The commands the webview calls for the config and the children.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::Serialize;
use tauri::{AppHandle, State};

use crate::config::{self, AppConfig, Mode, StartView};
use crate::login::{self, LoginGate, Password};
use crate::model::{ModelControl, ModelState};
use crate::poller::Poller;
use crate::processes::{ProcessStates, Processes};
use crate::status::Status;
use crate::token::{KeychainStore, TokenStore, token_key};
use crate::windows::{self, WindowKind};

/// How the config is written to its folder: [`config::save`], or a slow one in tests.
pub type SaveFn = Arc<dyn Fn(&Path, &AppConfig) -> std::io::Result<()> + Send + Sync>;

/// The config after a change, and a Keychain problem that the change could not fix (an old
/// login that could not be removed), as a sentence to show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigAnswer {
    pub config: AppConfig,
    pub keychain: Option<String>,
}

/// Why [`ConfigState::try_update`] changed nothing.
pub enum TryUpdateError<E> {
    /// The change refused.
    Refused(E),
    /// The save failed.
    NotSaved(String),
}

/// The live config and the folder it is saved in.
#[derive(Clone)]
pub struct ConfigState {
    pub dir: PathBuf,
    /// Held only to read or swap the config, never during a save.
    pub config: Arc<Mutex<AppConfig>>,
    /// The file was broken and set aside at start; cleared by the next save.
    reset: Arc<AtomicBool>,
    /// One change at a time, across its save, so two changes never lose one another.
    saving: Arc<Mutex<()>>,
    save: SaveFn,
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
        Self::load_with(dir, Arc::new(config::save))
    }

    /// [`load`](Self::load) with another way to save.
    pub fn load_with(dir: PathBuf, save: SaveFn) -> Self {
        let loaded = config::load_checked(&dir);
        Self {
            dir,
            config: Arc::new(Mutex::new(loaded.config)),
            reset: Arc::new(AtomicBool::new(loaded.reset)),
            saving: Arc::default(),
            save,
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
    /// The save (with its fsync) runs outside the config's lock, so a read never waits for
    /// it; the new config is swapped in under a short lock afterwards.
    fn update(&self, change: impl FnOnce(&mut AppConfig)) -> Result<AppConfig, String> {
        self.try_update(|c| {
            change(c);
            Ok::<(), ()>(())
        })
        .map(|(_, next)| next)
        .map_err(|e| match e {
            TryUpdateError::NotSaved(why) => why,
            TryUpdateError::Refused(()) => unreachable!("the change never refuses"),
        })
    }

    /// [`update`](Self::update) with a change that may refuse, made while no other change
    /// runs. Answers the config it replaced and the new one.
    pub fn try_update<E>(
        &self,
        change: impl FnOnce(&mut AppConfig) -> Result<(), E>,
    ) -> Result<(AppConfig, AppConfig), TryUpdateError<E>> {
        let _saving = self.saving.lock().unwrap_or_else(PoisonError::into_inner);
        let before = self.current();
        let mut next = before.clone();
        change(&mut next).map_err(TryUpdateError::Refused)?;
        (self.save)(&self.dir, &next)
            .map_err(|e| TryUpdateError::NotSaved(format!("the config was not saved: {e}")))?;
        *self.config.lock().unwrap_or_else(PoisonError::into_inner) = next.clone();
        self.reset.store(false, Ordering::SeqCst);
        Ok((before, next))
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
        self.set_mode_replacing(mode, data_dir, server_url)
            .map(|(_, next)| next)
    }

    /// [`set_mode`](Self::set_mode) that also answers the config it replaced, read under the
    /// same lock as the change.
    pub fn set_mode_replacing(
        &self,
        mode: Mode,
        data_dir: Option<PathBuf>,
        server_url: Option<String>,
    ) -> Result<(AppConfig, AppConfig), String> {
        let server_url = server_url
            .map(|u| u.trim().to_owned())
            .filter(|u| !u.is_empty());
        if mode == Mode::Remote && !server_url.as_deref().is_some_and(is_web_address) {
            return Err(NEEDS_ADDRESS.to_owned());
        }
        let data_dir = data_dir.map(|d| expand_home(&d)).transpose()?;
        self.try_update(|c| {
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
            Ok::<(), std::convert::Infallible>(())
        })
        .map_err(|e| match e {
            TryUpdateError::NotSaved(why) => why,
            TryUpdateError::Refused(never) => match never {},
        })
    }

    pub fn set_start_view(&self, view: StartView) -> Result<AppConfig, String> {
        self.update(|c| c.start_view = view)
    }

    /// Forgets the account and the e-mail (Log out).
    pub fn forget_login(&self) -> Result<AppConfig, String> {
        self.update(|c| {
            c.account = None;
            c.email = None;
        })
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

/// An `http://` or `https://` address with a host and without a user or a password: a login
/// in the address would end up in the config file and in status lines.
fn is_web_address(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    else {
        return false;
    };
    // The URL parser reads `http:///x` as the host `x`; the text must name the host itself.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return false;
    }
    let host = authority;
    let named = !host.is_empty() && !host.starts_with(':');
    named && reqwest::Url::parse(url).is_ok_and(|u| u.host_str().is_some_and(|h| !h.is_empty()))
}

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

/// Log out: deletes the token of the account in the config, then forgets the account and
/// the e-mail. The answer is the config without them. It takes the login lock: a log out
/// during a login waits for it and then removes the token it left, so none stays behind.
pub fn log_out_of(
    gate: &LoginGate,
    store: &dyn TokenStore,
    state: &ConfigState,
) -> Result<AppConfig, String> {
    let _no_login = gate.hold();
    if let Some(key) = token_key(&state.current()) {
        store.delete(&key).map_err(|e| e.to_string())?;
    }
    state.forget_login()
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

/// Saves the new place of the server off the calling thread ([`change_place`]); when the
/// mode, the folder or the address changed, runs `restart` on a blocking thread of its own and
/// waits for it. A refused change restarts nothing.
pub async fn set_mode_then(
    state: &ConfigState,
    gate: Arc<LoginGate>,
    store: Arc<dyn TokenStore>,
    mode: Mode,
    data_dir: Option<PathBuf>,
    server_url: Option<String>,
    restart: impl FnOnce() + Send + 'static,
) -> Result<ConfigAnswer, String> {
    let placing = state.clone();
    let (before, answer) = tauri::async_runtime::spawn_blocking(move || {
        change_place(&gate, &*store, &placing, mode, data_dir, server_url)
    })
    .await
    .map_err(|e| e.to_string())??;
    let config = &answer.config;
    let changed = (before.mode, &before.data_dir, &before.server_url)
        != (config.mode, &config.data_dir, &config.server_url);
    if changed {
        tauri::async_runtime::spawn_blocking(restart)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(answer)
}

/// Saves the new place. When that forgets the account (another server), its token goes too,
/// so no write token stays where Log out cannot reach it. It takes the login lock, so it never
/// meets a running login half way. Answers the config it replaced and the new one.
pub fn change_place(
    gate: &LoginGate,
    store: &dyn TokenStore,
    state: &ConfigState,
    mode: Mode,
    data_dir: Option<PathBuf>,
    server_url: Option<String>,
) -> Result<(AppConfig, ConfigAnswer), String> {
    let _no_login = gate.hold();
    let (replaced, config) = state.set_mode_replacing(mode, data_dir, server_url)?;
    let mut keychain = None;
    if replaced.account.is_some()
        && config.account.is_none()
        && let Some(key) = token_key(&replaced)
        && let Err(e) = store.delete(&key)
    {
        keychain = Some(login::old_login_kept(&e.0));
    }
    Ok((replaced, ConfigAnswer { config, keychain }))
}

/// Sets where the server runs (the mode control on the Server page). When the mode, the folder
/// or the address changed, both children start afresh for it, off the main thread: the start
/// first stops what runs.
#[tauri::command]
pub async fn set_mode(
    state: State<'_, ConfigState>,
    processes: State<'_, Arc<Processes>>,
    gate: State<'_, Arc<LoginGate>>,
    mode: Mode,
    data_dir: Option<PathBuf>,
    server_url: Option<String>,
) -> Result<ConfigAnswer, String> {
    let processes = processes.inner().clone();
    let reader = state.reader();
    // The start reads the config inside its lock: the newest place wins.
    set_mode_then(
        &state,
        gate.inner().clone(),
        Arc::new(KeychainStore::new()),
        mode,
        data_dir,
        server_url,
        move || processes.start_with(|| reader.current()),
    )
    .await
}

/// Saves the folder view, off the main thread: the save waits for the disk.
#[tauri::command]
pub async fn set_start_view(
    state: State<'_, ConfigState>,
    start_view: StartView,
) -> Result<AppConfig, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.set_start_view(start_view))
        .await
        .map_err(|e| e.to_string())?
}

/// Starts both children afresh as the config asks (Start on the Server page). Off the main
/// thread: a start first stops what runs, which can take the grace.
#[tauri::command]
pub async fn start_processes(
    config: State<'_, ConfigState>,
    processes: State<'_, Arc<Processes>>,
) -> Result<ProcessStates, String> {
    let config = config.reader();
    let processes = processes.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        processes.start_with(|| config.current());
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

/// Log in with an e-mail and a password, off the main thread: the client is made, used and
/// dropped on a blocking thread. The answer is the config with the new account; the password
/// and the token never come back. Then the status polls again.
#[tauri::command]
pub async fn log_in(
    config: State<'_, ConfigState>,
    processes: State<'_, Arc<Processes>>,
    poller: State<'_, Poller>,
    gate: State<'_, Arc<LoginGate>>,
    email: String,
    password: String,
) -> Result<ConfigAnswer, String> {
    let state = config.inner().clone();
    let gate = gate.inner().clone();
    let serve_info = processes.serve_info();
    let password = Password::new(password);
    let saved = tauri::async_runtime::spawn_blocking(move || {
        login::log_in(
            &gate,
            &state,
            &serve_info,
            &KeychainStore::new(),
            &email,
            &password,
        )
        .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    poller.poll_now();
    Ok(saved)
}

/// Whether the Keychain keeps a login for the account in the config, off the main thread.
#[tauri::command]
pub async fn has_login(config: State<'_, ConfigState>) -> Result<bool, String> {
    let config = config.current();
    tauri::async_runtime::spawn_blocking(move || login::has_login(&KeychainStore::new(), &config))
        .await
        .map_err(|e| e.to_string())?
}

/// Log out: deletes the token from the Keychain and forgets the account, off the main thread
/// (where it may wait for a running login), then polls again.
#[tauri::command]
pub async fn log_out(
    config: State<'_, ConfigState>,
    poller: State<'_, Poller>,
    gate: State<'_, Arc<LoginGate>>,
) -> Result<AppConfig, String> {
    let state = config.inner().clone();
    let gate = gate.inner().clone();
    let after = tauri::async_runtime::spawn_blocking(move || {
        log_out_of(&gate, &KeychainStore::new(), &state)
    })
    .await
    .map_err(|e| e.to_string())??;
    poller.poll_now();
    Ok(after)
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

    /// A save that says when it starts and then waits for `release`.
    fn held_save(
        started: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) -> SaveFn {
        let release = Mutex::new(release);
        Arc::new(move |dir: &Path, config: &AppConfig| {
            let _ = started.send(());
            let _ = release
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(10));
            config::save(dir, config)
        })
    }

    #[test]
    fn reading_the_config_does_not_wait_for_a_save() {
        let dir = tempfile::tempdir().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let state =
            ConfigState::load_with(dir.path().to_owned(), held_save(started_tx, release_rx));
        let saver = state.clone();
        let saving = std::thread::spawn(move || saver.set_start_view(StartView::Columns));
        started_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the save did not start");
        // While the save runs, the config reads at once, as it was.
        let (read_tx, read_rx) = std::sync::mpsc::channel();
        let reader = state.clone();
        std::thread::spawn(move || {
            let _ = read_tx.send(reader.current());
        });
        let read = read_rx.recv_timeout(std::time::Duration::from_secs(2));
        release_tx.send(()).unwrap();
        assert_eq!(
            read.expect("the read waited for the save").start_view,
            StartView::Tree
        );
        assert_eq!(
            saving.join().unwrap().unwrap().start_view,
            StartView::Columns
        );
        assert_eq!(state.current().start_view, StartView::Columns);
    }

    #[test]
    fn changes_at_the_same_time_are_all_kept() {
        let dir = tempfile::tempdir().unwrap();
        let slow: SaveFn = Arc::new(|dir: &Path, config: &AppConfig| {
            std::thread::sleep(std::time::Duration::from_millis(30));
            config::save(dir, config)
        });
        let state = ConfigState::load_with(dir.path().to_owned(), slow);
        let a = state.clone();
        let b = state.clone();
        let view = std::thread::spawn(move || a.set_start_view(StartView::Columns));
        let mode =
            std::thread::spawn(move || b.set_mode(Mode::Local, Some("/data/x".into()), None));
        view.join().unwrap().unwrap();
        mode.join().unwrap().unwrap();
        for config in [state.current(), config::load(dir.path())] {
            assert_eq!(config.start_view, StartView::Columns);
            assert_eq!(config.mode, Mode::Local);
        }
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
    fn an_address_needs_a_host() {
        let dir = tempfile::tempdir().unwrap();
        let state = ConfigState::load(dir.path().to_owned());
        for url in ["http://", "https://", "http:///rest/v1", "https://:19072"] {
            assert_eq!(
                state
                    .set_mode(Mode::Remote, None, Some(url.to_owned()))
                    .unwrap_err(),
                NEEDS_ADDRESS,
                "{url}"
            );
        }
        assert!(
            state
                .set_mode(Mode::Remote, None, Some("http://pi.local:19072".into()))
                .is_ok()
        );
    }

    #[test]
    fn an_address_with_a_login_in_it_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let state = ConfigState::load(dir.path().to_owned());
        for url in [
            "https://user:secret@n.example.org",
            "https://user@n.example.org",
            "http://user:secret@pi.local:19072/rest/v1",
            "http://@pi.local",
        ] {
            assert_eq!(
                state
                    .set_mode(Mode::Remote, None, Some(url.to_owned()))
                    .unwrap_err(),
                NEEDS_ADDRESS,
                "{url}"
            );
        }
        assert_eq!(state.current().server_url, None);
        // An @ after the host is no login.
        assert!(
            state
                .set_mode(
                    Mode::Remote,
                    None,
                    Some("https://n.example.org/x?to=a@b".into())
                )
                .is_ok()
        );
    }

    #[test]
    fn a_new_place_restarts_the_children_off_the_calling_thread() {
        let dir = tempfile::tempdir().unwrap();
        let state = ConfigState::load(dir.path().to_owned());
        let caller = std::thread::current().id();
        let runs: Arc<Mutex<Vec<(std::thread::ThreadId, AppConfig)>>> = Arc::default();
        let restart = |state: &ConfigState| {
            let runs = runs.clone();
            let reader = state.reader();
            move || {
                runs.lock()
                    .unwrap()
                    .push((std::thread::current().id(), reader.current()))
            }
        };
        let set = |mode, dir: &str| {
            tauri::async_runtime::block_on(set_mode_then(
                &state,
                Arc::new(LoginGate::default()),
                Arc::new(crate::token::MemoryStore::new()),
                mode,
                Some(dir.into()),
                None,
                restart(&state),
            ))
            .unwrap()
            .config
        };
        set(Mode::Local, "/data/a");
        // The same place again restarts nothing.
        set(Mode::Local, "/data/a");
        set(Mode::Local, "/data/b");
        let runs = runs.lock().unwrap();
        assert_eq!(runs.len(), 2);
        for (thread, _) in runs.iter() {
            assert_ne!(*thread, caller);
        }
        // The restart reads the config as it is then.
        assert_eq!(runs[1].1.data_dir, PathBuf::from("/data/b"));
        // A refused change restarts nothing.
        drop(runs);
        assert!(
            tauri::async_runtime::block_on(set_mode_then(
                &state,
                Arc::new(LoginGate::default()),
                Arc::new(crate::token::MemoryStore::new()),
                Mode::Remote,
                None,
                None,
                restart(&state)
            ))
            .is_err()
        );
    }

    #[test]
    fn log_out_deletes_the_token_and_forgets_the_account() {
        use crate::token::{MemoryStore, Token, TokenStore};
        let dir = tempfile::tempdir().unwrap();
        let config = AppConfig {
            mode: Mode::Local,
            account: Some("jordi".into()),
            email: Some("me@example.org".into()),
            ..AppConfig::default()
        };
        config::save(dir.path(), &config).unwrap();
        let state = ConfigState::load(dir.path().to_path_buf());
        let store = MemoryStore::new();
        store.set("local|jordi", &Token::new("secret")).unwrap();
        store.set("local|other", &Token::new("other")).unwrap();
        let gate = LoginGate::default();
        let after = log_out_of(&gate, &store, &state).unwrap();
        assert_eq!(store.get("local|jordi").unwrap(), None);
        assert!(store.get("local|other").unwrap().is_some());
        assert_eq!(after.account, None);
        assert_eq!(after.email, None);
        assert_eq!(after.mode, Mode::Local);
        assert_eq!(state.current(), after);
        assert_eq!(config::load(dir.path()), after);
        // Without an account there is nothing to delete.
        log_out_of(&gate, &store, &state).unwrap();
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
