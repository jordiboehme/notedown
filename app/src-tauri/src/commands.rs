//! The commands the webview calls for the config and the children.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::Serialize;
use tauri::{AppHandle, State};

use crate::config::{self, AppConfig, Mode, StartView};
use crate::import_job::{self, BackupFolder, ImportJobs, JobKind};
use crate::library::{Choice, Entry, Job, Key, Library, Listed, RecyclePage, StateView, UploadAsk};
use crate::login::{self, LoginError, LoginGate, Password};
use crate::model::{ModelControl, ModelState};
use crate::notes::{NoteSummary, Notes};
use crate::poller::{Poller, rest_base};
use crate::processes::{ProcessStates, Processes};
use crate::review::{DataUrl, NoteView, Review};
use crate::serve_info::ServeInfo;
use crate::setup::{self, ExistingFolder, Sidecar};
use crate::status::Status;
use crate::token::{KeychainStore, Token, TokenStore, token_key};
use crate::windows::{self, Opening, WindowKind};

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
            // The first change to this Mac turns Start at login on, once ([`set_mode_then`]).
            if before.0 != Mode::Local && c.mode == Mode::Local && !c.setup.autostart_offered {
                c.setup.autostart_offered = true;
            }
            // A kept account without a Markdown folder in the app gets the pipeline's, before
            // the children start again with `--markdown-dir`.
            c.seed_markdown_dir();
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

    /// Gives the logged-in account the Markdown folder of the pipeline's own config when the
    /// app has none for it yet ([`AppConfig::seed_markdown_dir`]), and saves it. Runs before
    /// the pipeline starts with `--markdown-dir` (at the start of the app and at a login), so
    /// the flag never replaces a folder the user set up for the pipeline. Nothing is saved
    /// when there is nothing to add.
    pub fn seed_markdown_dir(&self) -> Result<AppConfig, String> {
        match self.try_update(|c| {
            if c.seed_markdown_dir() {
                Ok(())
            } else {
                Err(())
            }
        }) {
            Ok((_, next)) => Ok(next),
            Err(TryUpdateError::Refused(())) => Ok(self.current()),
            Err(TryUpdateError::NotSaved(why)) => Err(why),
        }
    }

    /// Sets the Markdown folder of the logged-in account. The folder must be a full path (a
    /// leading `~` is the home folder) outside the data folder: the pipeline refuses a folder
    /// inside it. The check is [`inside`]: as written, as the pipeline compares, and also with
    /// the links resolved. So the app is stricter than the pipeline: it also refuses a folder
    /// that reaches into the data folder through a link, which the pipeline would take.
    pub fn set_markdown_dir(&self, dir: PathBuf) -> Result<AppConfig, String> {
        let dir = without_dots(&expand_home(&dir)?);
        self.try_update(|c| {
            let slug = c.account_slug().ok_or(NEEDS_ACCOUNT)?.to_owned();
            if inside(&dir, &without_dots(&c.data_dir)) {
                return Err(OUTSIDE_DATA);
            }
            c.markdown_dirs.insert(slug, dir);
            Ok(())
        })
        .map(|(_, next)| next)
        .map_err(|e| match e {
            TryUpdateError::Refused(text) => text.to_owned(),
            TryUpdateError::NotSaved(why) => why,
        })
    }

    /// Saves what the settings window chose for the backup and the import, and that the user
    /// saw the deletes of the first sync. A folder or a file must be a full path (a leading `~`
    /// is the home folder).
    pub fn set_setup_record(&self, mut change: config::SetupChange) -> Result<AppConfig, String> {
        for path in [&mut change.backup_dir, &mut change.snbak] {
            if let Some(Some(p)) = path {
                *p = expand_home(p)?;
            }
        }
        self.update(|c| change.apply(&mut c.setup))
    }

    /// Saves the sync mark of a new import of the logged-in account (`mark.key` must be its
    /// token key). It replaces an older mark, also of the same account, and the check of the
    /// first sync after the old one. Answers whether it was saved.
    pub fn set_sync_mark(&self, mark: config::SyncMark) -> Result<bool, String> {
        self.save_mark(mark, true)
    }

    /// [`set_sync_mark`](Self::set_sync_mark) only when there is no mark for that account
    /// yet (the import on another machine, seen by the poller). A mark of another key is
    /// replaced; a mark is kept over a log out and used only while its key is the login's.
    pub fn take_sync_mark(&self, mark: config::SyncMark) -> Result<bool, String> {
        self.save_mark(mark, false)
    }

    fn save_mark(&self, mark: config::SyncMark, replace_own: bool) -> Result<bool, String> {
        match self.try_update(|c| {
            let same = token_key(c).as_deref() == Some(mark.key.as_str());
            let has = c
                .setup
                .sync_mark
                .as_ref()
                .is_some_and(|m| m.key == mark.key);
            if !same || (has && !replace_own) {
                return Err(());
            }
            c.setup.forget_sync();
            c.setup.sync_mark = Some(mark);
            Ok(())
        }) {
            Ok(_) => Ok(true),
            Err(TryUpdateError::Refused(())) => Ok(false),
            Err(TryUpdateError::NotSaved(why)) => Err(why),
        }
    }

    /// Both imports are done: the backup folder and the `.snbak` are no longer needed.
    pub fn imports_done(&self) -> Result<AppConfig, String> {
        self.update(|c| {
            c.setup.backup_dir = None;
            c.setup.snbak = None;
        })
    }

    /// Forgets the account and the e-mail (Log out).
    pub fn forget_login(&self) -> Result<AppConfig, String> {
        self.update(|c| {
            c.account = None;
            c.email = None;
        })
    }
}

/// What the Markdown folder refuses without an account.
pub const NEEDS_ACCOUNT: &str = "Log in to choose the Markdown folder.";
/// What the Markdown folder refuses inside the data folder: the pipeline would refuse it too.
pub const OUTSIDE_DATA: &str = "Choose a folder outside the Notedown data folder.";

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

/// The setup's links to the instructions on GitHub: getting started, the server on another
/// machine and the pipeline on a Mac.
pub const SETUP_LINKS: [&str; 3] = [
    "https://github.com/jordiboehme/notedown/blob/main/docs/getting-started.md",
    "https://github.com/jordiboehme/notedown/blob/main/docs/getting-started.md#the-server-on-another-machine",
    "https://github.com/jordiboehme/notedown/blob/main/docs/getting-started.md#a-pipeline-on-a-mac",
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
/// The library binds again: its jobs end and it is logged out. Then the server the token
/// belongs to (`serve_info` finds the app's own) is asked to revoke it, best effort, on a
/// thread of its own and within [`login::LOGOUT_TIMEOUT`]: the answer does not wait for it,
/// and a server that is off or too old still leaves this Mac logged out.
pub fn log_out_of(
    gate: &LoginGate,
    store: &dyn TokenStore,
    state: &ConfigState,
    library: &Library,
    serve_info: &ServeInfo,
) -> Result<AppConfig, String> {
    let (forgotten, revoking) = {
        let _no_login = gate.hold();
        let before = state.current();
        let mut revoking = None;
        if let Some(key) = token_key(&before) {
            // A token the Keychain cannot read is not revoked; the delete still runs.
            let token = store.get(&key).ok().flatten();
            store.delete(&key).map_err(|e| e.to_string())?;
            revoking = token.zip(rest_base(&before, serve_info));
        }
        (state.forget_login(), revoking)
    };
    // The token is gone also when the config was not saved. After the login lock, as after a
    // login: the binding reads the Keychain.
    library.relogin();
    // On a thread of its own: this Mac is logged out already, and a silent server must not
    // hold the answer up. The call itself ends within the bound.
    if revoking.is_some() {
        std::thread::spawn(move || revoke_quietly(revoking));
    }
    forgotten
}

/// Revokes a token that this Mac let go of; the outcome is not shown and never logged.
fn revoke_quietly(revoking: Option<(Token, String)>) {
    if let Some((token, base)) = revoking {
        let _ = login::revoke(&base, token);
    }
}

/// Log in ([`login::log_in`]), then bind the library to the new login.
pub fn log_in_to(
    gate: &LoginGate,
    state: &ConfigState,
    serve_info: &ServeInfo,
    store: &dyn TokenStore,
    library: &Library,
    email: &str,
    password: &Password,
) -> Result<ConfigAnswer, LoginError> {
    let answer = login::log_in(gate, state, serve_info, store, email, password)?;
    library.relogin();
    Ok(answer)
}

/// An absolute path with `.` and `..` resolved as written, without asking the disk (as the
/// pipeline's own check does).
fn without_dots(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Whether `dir` is `data` or lies inside it: as written (`.` and `..` resolved, as the
/// pipeline compares), or after the links are resolved (a symlinked folder, `/tmp` and
/// `/private/tmp`, the case of an APFS name). Of a path that is not there yet, its deepest
/// existing folder is resolved and the rest added as written.
fn inside(dir: &Path, data: &Path) -> bool {
    dir.starts_with(data) || resolved(dir).starts_with(resolved(data))
}

/// `path` with the links of its deepest existing part resolved; as it is when none resolves.
fn resolved(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut part = path;
    loop {
        if let Ok(real) = std::fs::canonicalize(part) {
            return rest.iter().rev().fold(real, |acc, name| acc.join(name));
        }
        match (part.parent(), part.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                part = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

pub fn allowed_link(url: &str) -> bool {
    ABOUT_LINKS.contains(&url) || SETUP_LINKS.contains(&url)
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
/// waits for it. A refused change restarts nothing. `serve_info` is the app's own server as it
/// runs now: the old place, whose token a new server revokes before the restart stops it.
/// `autostart` turns Start at login on; it runs once, after the first change to Local mode.
#[allow(clippy::too_many_arguments)]
pub async fn set_mode_then(
    state: &ConfigState,
    gate: Arc<LoginGate>,
    store: Arc<dyn TokenStore>,
    library: Library,
    mode: Mode,
    data_dir: Option<PathBuf>,
    server_url: Option<String>,
    restart: impl FnOnce() + Send + 'static,
    serve_info: &ServeInfo,
    autostart: impl FnOnce() + Send + 'static,
) -> Result<ConfigAnswer, String> {
    let placing = state.clone();
    let serve_info = serve_info.clone();
    let (before, answer) = tauri::async_runtime::spawn_blocking(move || {
        change_place(
            &gate,
            &*store,
            &placing,
            &library,
            mode,
            data_dir,
            server_url,
            &serve_info,
        )
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
    // This change was the first to this Mac: Start at login goes on, once.
    if !before.setup.autostart_offered && config.setup.autostart_offered {
        tauri::async_runtime::spawn_blocking(autostart)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(answer)
}

/// Saves the new place. When that forgets the account (another server), its token goes too,
/// so no write token stays where Log out cannot reach it. It takes the login lock, so it never
/// meets a running login half way. Answers the config it replaced and the new one. The library
/// binds again when its server or its account changed. The old server (`serve_info` finds the
/// app's own) is asked to revoke the token, best effort, as by Log out. Unlike Log out it
/// waits for the revoke (at most [`login::LOGOUT_TIMEOUT`]): the restart that follows stops
/// the app's own server, which must hear of it first.
#[allow(clippy::too_many_arguments)]
pub fn change_place(
    gate: &LoginGate,
    store: &dyn TokenStore,
    state: &ConfigState,
    library: &Library,
    mode: Mode,
    data_dir: Option<PathBuf>,
    server_url: Option<String>,
    serve_info: &ServeInfo,
) -> Result<(AppConfig, ConfigAnswer), String> {
    let (replaced, config, keychain, revoking) = {
        let _no_login = gate.hold();
        let (replaced, config) = state.set_mode_replacing(mode, data_dir, server_url)?;
        let mut keychain = None;
        let mut revoking = None;
        if replaced.account.is_some()
            && config.account.is_none()
            && let Some(key) = token_key(&replaced)
        {
            let token = store.get(&key).ok().flatten();
            revoking = token.zip(rest_base(&replaced, serve_info));
            if let Err(e) = store.delete(&key) {
                keychain = Some(login::old_login_kept(&e.0));
            }
        }
        (replaced, config, keychain, revoking)
    };
    // After the login lock, as after a login: the binding reads the Keychain.
    library.refresh();
    revoke_quietly(revoking);
    Ok((replaced, ConfigAnswer { config, keychain }))
}

/// Sets where the server runs (the mode control on the Server page). When the mode, the folder
/// or the address changed, both children start afresh for it, off the main thread: the start
/// first stops what runs.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn set_mode(
    app: AppHandle,
    state: State<'_, ConfigState>,
    processes: State<'_, Arc<Processes>>,
    gate: State<'_, Arc<LoginGate>>,
    library: State<'_, Library>,
    jobs: State<'_, ImportJobs>,
    mode: Mode,
    data_dir: Option<PathBuf>,
    server_url: Option<String>,
) -> Result<ConfigAnswer, String> {
    let _server = jobs.use_server()?;
    let processes = processes.inner().clone();
    let serve_info = processes.serve_info();
    let reader = state.reader();
    // The start reads the config inside its lock: the newest place wins.
    set_mode_then(
        &state,
        gate.inner().clone(),
        Arc::new(KeychainStore::new()),
        library.inner().clone(),
        mode,
        data_dir,
        server_url,
        move || processes.start_with(|| reader.current()),
        &serve_info,
        move || {
            use tauri_plugin_autostart::ManagerExt;
            if let Err(e) = app.autolaunch().enable() {
                eprintln!("notedown: Start at login could not be turned on: {e}");
            }
        },
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
    jobs: State<'_, ImportJobs>,
) -> Result<ProcessStates, String> {
    let _server = jobs.use_server()?;
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

/// Opens the library window: at the review of the note at `note` (its path) when given, at New
/// with `new_notes`, else as it is. Async, so the window is made off the main thread.
#[tauri::command]
pub async fn open_library(
    app: AppHandle,
    note: Option<String>,
    new_notes: Option<bool>,
) -> Result<(), String> {
    let opening = windows::library_opening(note, new_notes);
    windows::show_window(&app, WindowKind::Library, opening).map_err(|e| e.to_string())
}

/// Opens the settings window, at `page` when given.
#[tauri::command]
pub async fn open_settings(app: AppHandle, page: Option<String>) -> Result<(), String> {
    let opening = page.map_or(Opening::Plain, Opening::Page);
    windows::show_window(&app, WindowKind::Settings, opening).map_err(|e| e.to_string())
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
/// and the token never come back. Then the library binds to the new login and the status polls
/// again.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn log_in(
    config: State<'_, ConfigState>,
    processes: State<'_, Arc<Processes>>,
    poller: State<'_, Poller>,
    gate: State<'_, Arc<LoginGate>>,
    library: State<'_, Library>,
    jobs: State<'_, ImportJobs>,
    email: String,
    password: String,
) -> Result<ConfigAnswer, String> {
    let server = jobs.use_server()?;
    let state = config.inner().clone();
    let gate = gate.inner().clone();
    let library = library.inner().clone();
    let serve_info = processes.serve_info();
    let password = Password::new(password);
    let saved = tauri::async_runtime::spawn_blocking(move || {
        log_in_to(
            &gate,
            &state,
            &serve_info,
            &KeychainStore::new(),
            &library,
            &email,
            &password,
        )
        .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())??;
    relaunch_pipeline(config.inner(), processes.inner(), server);
    poller.poll_now();
    Ok(saved)
}

/// Starts the pipeline again when its arguments changed (another Markdown folder, or another
/// account: the folder belongs to it), on a blocking thread that nobody waits for: the stop of
/// the old pipeline can take its grace. The config is read inside the start's lock, so the
/// newest wins.
///
/// `server` is the command's use of the server ([`ImportJobs::use_server`]): it lasts until the
/// relaunch is over, so no import stops the children in the middle of it.
pub(crate) fn relaunch_pipeline(
    config: &ConfigState,
    processes: &Arc<Processes>,
    server: import_job::UseGuard,
) {
    let reader = config.reader();
    let processes = processes.clone();
    tauri::async_runtime::spawn_blocking(move || {
        processes.relaunch_pipeline_with(|| reader.current());
        drop(server);
    });
}

/// Whether the Keychain keeps a login for the account in the config, off the main thread.
#[tauri::command]
pub async fn has_login(config: State<'_, ConfigState>) -> Result<bool, String> {
    let config = config.current();
    tauri::async_runtime::spawn_blocking(move || login::has_login(&KeychainStore::new(), &config))
        .await
        .map_err(|e| e.to_string())?
}

/// Log out: deletes the token from the Keychain, forgets the account, stops the library and
/// asks the server to revoke the token (without waiting for it), off the main thread (where
/// it may wait for a running login), then polls again.
#[tauri::command]
pub async fn log_out(
    config: State<'_, ConfigState>,
    processes: State<'_, Arc<Processes>>,
    poller: State<'_, Poller>,
    gate: State<'_, Arc<LoginGate>>,
    library: State<'_, Library>,
    jobs: State<'_, ImportJobs>,
) -> Result<AppConfig, String> {
    let server = jobs.use_server()?;
    let state = config.inner().clone();
    let gate = gate.inner().clone();
    let library = library.inner().clone();
    let serve_info = processes.serve_info();
    let after = tauri::async_runtime::spawn_blocking(move || {
        log_out_of(&gate, &KeychainStore::new(), &state, &library, &serve_info)
    })
    .await
    .map_err(|e| e.to_string())??;
    relaunch_pipeline(config.inner(), processes.inner(), server);
    poller.poll_now();
    Ok(after)
}

/// Polls the server and the pipeline now (Check again on the Setup page). The new status
/// comes as an event.
#[tauri::command]
pub async fn recheck(poller: State<'_, Poller>) -> Result<(), String> {
    let poller = poller.inner().clone();
    tauri::async_runtime::spawn_blocking(move || poller.poll_now())
        .await
        .map_err(|e| e.to_string())
}

/// Makes an account on this Mac's server (Local mode only) with the sidecar, then logs in with
/// it as Log in does, off the main thread. The password goes to the sidecar over stdin only.
/// When the account was made but the login failed, the error says to log in with it.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn create_account(
    config: State<'_, ConfigState>,
    processes: State<'_, Arc<Processes>>,
    poller: State<'_, Poller>,
    gate: State<'_, Arc<LoginGate>>,
    library: State<'_, Library>,
    sidecar: State<'_, Sidecar>,
    jobs: State<'_, ImportJobs>,
    slug: String,
    email: String,
    password: String,
) -> Result<ConfigAnswer, String> {
    let server = jobs.use_server()?;
    let state = config.inner().clone();
    let gate = gate.inner().clone();
    let library = library.inner().clone();
    let spawn = sidecar.0.clone();
    let serve_info = processes.serve_info();
    let password = Password::new(password);
    let answer = tauri::async_runtime::spawn_blocking(move || {
        setup::create_and_log_in(
            &spawn,
            &gate,
            &state,
            &serve_info,
            &KeychainStore::new(),
            &library,
            &slug,
            &email,
            &password,
        )
    })
    .await
    .map_err(|e| e.to_string())?;
    // Also after a failed login: the server has the account now.
    relaunch_pipeline(config.inner(), processes.inner(), server);
    poller.poll_now();
    answer
}

/// Connects this Mac's pipeline to the server on another machine (Remote mode only) with the
/// config's address, folder and e-mail, off the main thread; then the pipeline starts and the
/// status polls again. The password goes to the sidecar over stdin only.
#[tauri::command]
pub async fn connect_pipeline(
    config: State<'_, ConfigState>,
    processes: State<'_, Arc<Processes>>,
    poller: State<'_, Poller>,
    sidecar: State<'_, Sidecar>,
    jobs: State<'_, ImportJobs>,
    password: String,
) -> Result<ConfigAnswer, String> {
    let server = jobs.use_server()?;
    let current = config.current();
    let spawn = sidecar.0.clone();
    let password = Password::new(password);
    tauri::async_runtime::spawn_blocking(move || {
        setup::connect_configured(&spawn, &current, &password)
    })
    .await
    .map_err(|e| e.to_string())??;
    relaunch_pipeline(config.inner(), processes.inner(), server);
    poller.poll_now();
    Ok(ConfigAnswer {
        config: config.current(),
        keychain: None,
    })
}

/// What the config's data folder, `~/notedown-data` and `~/notedown-mac` hold (a server with
/// its accounts, a pipeline with its server), off the main thread.
#[tauri::command]
pub async fn existing_details(
    config: State<'_, ConfigState>,
) -> Result<Vec<ExistingFolder>, String> {
    let dirs = setup::setup_folders(&config.current());
    tauri::async_runtime::spawn_blocking(move || setup::existing_details(&dirs))
        .await
        .map_err(|e| e.to_string())
}

/// The Markdown folder of the logged-in account (the OCR page); `null` without an account.
#[tauri::command]
pub fn markdown_dir(config: State<'_, ConfigState>) -> Option<String> {
    config
        .current()
        .markdown_dir()
        .map(|dir| dir.display().to_string())
}

/// Sets the Markdown folder of the logged-in account (Choose... on the OCR page) and answers
/// it. When the pipeline's arguments changed, the pipeline starts again with them on a thread of
/// its own after the answer (its stop can take the grace); the server runs on.
#[tauri::command]
pub async fn set_markdown_dir(
    config: State<'_, ConfigState>,
    processes: State<'_, Arc<Processes>>,
    jobs: State<'_, ImportJobs>,
    dir: PathBuf,
) -> Result<Option<String>, String> {
    let server = jobs.use_server()?;
    let state = config.inner().clone();
    let saved = tauri::async_runtime::spawn_blocking(move || state.set_markdown_dir(dir))
        .await
        .map_err(|e| e.to_string())??;
    relaunch_pipeline(config.inner(), processes.inner(), server);
    Ok(saved.markdown_dir().map(|dir| dir.display().to_string()))
}

/// Show in Finder for the Markdown folder: makes it when it is not there yet (the pipeline
/// makes it too, with its first note), then `/usr/bin/open -- <dir>`.
fn finder_command(dir: &Path) -> Result<std::process::Command, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("The folder could not be made: {e}"))?;
    let mut command = std::process::Command::new("/usr/bin/open");
    command.arg("--").arg(dir);
    Ok(command)
}

/// Opens the Markdown folder of the logged-in account in the Finder, off the main thread.
#[tauri::command]
pub async fn show_markdown_dir(config: State<'_, ConfigState>) -> Result<(), String> {
    let dir = config.current().markdown_dir().ok_or(NEEDS_ACCOUNT)?;
    tauri::async_runtime::spawn_blocking(move || {
        let status = finder_command(&dir)?.status().map_err(|e| e.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err("The Finder did not open.".to_owned())
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The state of the library, its text and its Settings page, for the library window when it
/// opens; `null` before the first binding (a `state` event follows). It reads only what the
/// library keeps: no call and no Keychain read.
#[tauri::command]
pub fn library_state(library: State<'_, Library>) -> Option<StateView> {
    library.state()
}

/// The folder at `path` as loaded so far, with the cursor of its next page: the first page
/// without `cursor`, else the page after it. `foldersOnly` lists only its folders (the tree).
/// The calls run off the main thread.
#[tauri::command]
pub async fn library_list(
    library: State<'_, Library>,
    path: String,
    cursor: Option<String>,
    folders_only: Option<bool>,
) -> Result<Listed, String> {
    let library = library.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        library.list(&path, cursor.as_deref(), folders_only.unwrap_or(false))
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

/// A listing the library window shows: a folder, or only its folders (the tree).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShownListing {
    pub path: String,
    #[serde(default)]
    pub folders_only: bool,
}

/// The listings the library window shows now, and whether it shows the recycle bin. The
/// library keeps and follows only these, and lists only these again after a gap or a long
/// pause; while the bin is shown it follows the events even without a listing, so a delete
/// from elsewhere sends `recycle`. Async, so it never waits on the main thread.
#[tauri::command]
pub async fn library_shown(
    library: State<'_, Library>,
    listings: Vec<ShownListing>,
    bin_shown: bool,
) -> Result<(), String> {
    let keys: Vec<Key> = listings
        .iter()
        .map(|l| Key::new(&l.path, l.folders_only))
        .collect();
    // The bin first: a step between the two calls never sees neither.
    library.set_bin_shown(bin_shown);
    library.set_shown(&keys);
    Ok(())
}

/// One file or folder by its id.
#[tauri::command]
pub async fn library_entry(library: State<'_, Library>, id: u64) -> Result<Entry, String> {
    let library = library.inner().clone();
    tauri::async_runtime::spawn_blocking(move || library.entry(id))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// A page of the recycle bin: the first one without `cursor`.
#[tauri::command]
pub async fn library_recycle(
    library: State<'_, Library>,
    cursor: Option<String>,
) -> Result<RecyclePage, String> {
    let library = library.inner().clone();
    tauri::async_runtime::spawn_blocking(move || library.recycle(cursor.as_deref()))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// Runs a change of the library off the main thread and answers its job as it ended.
async fn change(
    library: &Library,
    work: impl FnOnce(Library) -> Job + Send + 'static,
) -> Result<Job, String> {
    let library = library.clone();
    tauri::async_runtime::spawn_blocking(move || work(library))
        .await
        .map_err(|e| e.to_string())
}

/// Makes the folder `name` in `parent`, as a change job.
#[tauri::command]
pub async fn library_new_folder(
    library: State<'_, Library>,
    parent: String,
    name: String,
) -> Result<Job, String> {
    change(&library, move |l| l.new_folder(&parent, &name)).await
}

/// Renames the file or folder `id` to `name` in its folder, as a change job.
#[tauri::command]
pub async fn library_rename(
    library: State<'_, Library>,
    id: u64,
    name: String,
) -> Result<Job, String> {
    change(&library, move |l| l.rename(id, &name)).await
}

/// Moves the file or folder `id` into the folder `toFolder`, as a change job.
#[tauri::command]
pub async fn library_move(
    library: State<'_, Library>,
    id: u64,
    to_folder: String,
) -> Result<Job, String> {
    change(&library, move |l| l.move_to(id, &to_folder)).await
}

/// Moves the file or folder `id` to the recycle bin, as a change job.
#[tauri::command]
pub async fn library_delete(library: State<'_, Library>, id: u64) -> Result<Job, String> {
    change(&library, move |l| l.delete(id)).await
}

/// Brings the bin item `key` back, at `path` or where it was, as a change job. A taken path
/// answers the job as `asking` with the clash; the window asks for a name and calls again
/// with `path`, which goes on with the same job.
#[tauri::command]
pub async fn library_restore(
    library: State<'_, Library>,
    key: String,
    path: Option<String>,
) -> Result<Job, String> {
    change(&library, move |l| l.restore(&key, path.as_deref())).await
}

/// Removes every deleted item from the recycle bin for good, as a change job.
#[tauri::command]
pub async fn library_empty(library: State<'_, Library>) -> Result<Job, String> {
    change(&library, |l| l.empty_bin()).await
}

/// Uploads the drop `drop_id` (files and folders dropped on the library window; Rust kept their
/// paths) into `folder`. Answers a question to ask first, or none when the jobs are queued: the
/// window then follows their `job` events. Call it again with `many` after "Upload 1,234
/// files?", and with `packages` (true: go into them, false: leave them out) after the package
/// question.
#[tauri::command]
pub async fn library_upload(
    library: State<'_, Library>,
    drop_id: u64,
    folder: String,
    many: Option<bool>,
    packages: Option<bool>,
) -> Result<Option<UploadAsk>, String> {
    let library = library.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        library.upload(drop_id, &folder, many.unwrap_or(false), packages)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The answer to an upload that asks about a name in the way: replace, keepBoth or skip.
#[tauri::command]
pub async fn library_answer(
    library: State<'_, Library>,
    job: u64,
    choice: Choice,
) -> Result<(), String> {
    library.answer(job, choice)
}

/// Tries a failed upload again.
#[tauri::command]
pub async fn library_retry(library: State<'_, Library>, job: u64) -> Result<(), String> {
    library.retry(job)
}

/// Cancels a job: an upload wherever it is, or a restore that asks for a name.
#[tauri::command]
pub async fn library_cancel(library: State<'_, Library>, job: u64) -> Result<(), String> {
    library.cancel(job);
    Ok(())
}

/// Cancel all: every job of the drop `drop_id` ends, and an unused drop is forgotten.
#[tauri::command]
pub async fn library_cancel_drop(library: State<'_, Library>, drop_id: u64) -> Result<(), String> {
    library.cancel_drop(drop_id);
    Ok(())
}

/// Downloads the file `id` into the Downloads folder: answers the queued `download` job, whose
/// events follow. Retry and Cancel are `library_retry` and `library_cancel`.
#[tauri::command]
pub async fn library_download(library: State<'_, Library>, id: u64) -> Result<Job, String> {
    let library = library.inner().clone();
    tauri::async_runtime::spawn_blocking(move || library.download(id))
        .await
        .map_err(|e| e.to_string())?
}

/// Show in Finder: `/usr/bin/open -R -- <path>`, only for a file Notedown downloaded in this
/// run, by the path its job answered.
#[tauri::command]
pub async fn library_reveal(library: State<'_, Library>, path: String) -> Result<(), String> {
    let library = library.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let status = library
            .reveal_command(&path)?
            .status()
            .map_err(|e| e.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err("The Finder did not open.".to_owned())
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The notes with text the user has not opened since it came, newest first (the panel and the
/// library). It scans the store first, off the main thread.
#[tauri::command]
pub async fn notes_new(notes: State<'_, Notes>) -> Result<Vec<NoteSummary>, String> {
    let notes = notes.inner().clone();
    tauri::async_runtime::spawn_blocking(move || notes.new_notes())
        .await
        .map_err(|e| e.to_string())
}

/// Every note with text, newest first (the library's All notes). It scans the store first, off
/// the main thread.
#[tauri::command]
pub async fn notes_all(notes: State<'_, Notes>) -> Result<Vec<NoteSummary>, String> {
    let notes = notes.inner().clone();
    tauri::async_runtime::spawn_blocking(move || notes.all())
        .await
        .map_err(|e| e.to_string())
}

/// The library opened the note `key`: its text up to now is seen. Off the main thread.
#[tauri::command]
pub async fn note_seen(notes: State<'_, Notes>, key: String) -> Result<(), String> {
    let notes = notes.inner().clone();
    tauri::async_runtime::spawn_blocking(move || notes.seen(&key))
        .await
        .map_err(|e| e.to_string())?
}

/// The note at `path` for the review: the Markdown of each page from the pipeline's store, and
/// whether it is catching up. Off the main thread (it scans the store and asks the server).
#[tauri::command]
pub async fn note_open(review: State<'_, Review>, path: String) -> Result<NoteView, String> {
    let review = review.inner().clone();
    tauri::async_runtime::spawn_blocking(move || review.open(&path))
        .await
        .map_err(|e| e.to_string())?
}

/// Page `index` (0-based) of the note at `path` as a `data:image/png;base64,...` URL, rendered
/// alone when it is not cached. Off the main thread: it may download the note and render.
#[tauri::command]
pub async fn note_page_image(
    review: State<'_, Review>,
    path: String,
    index: usize,
) -> Result<DataUrl, String> {
    let review = review.inner().clone();
    tauri::async_runtime::spawn_blocking(move || review.page_image(&path, index))
        .await
        .map_err(|e| e.to_string())?
}

/// The review of the note at `path` closed: its waiting calls end and its renders that no
/// other review waits for are killed. Off the main thread.
#[tauri::command]
pub async fn note_close(review: State<'_, Review>, path: String) -> Result<(), String> {
    let review = review.inner().clone();
    tauri::async_runtime::spawn_blocking(move || review.close(&path))
        .await
        .map_err(|e| e.to_string())
}

/// The review of the note at `path` does not show page `index` anymore (it went far out of
/// view): its waiting call ends and its render stops when no other review waits for it. Off
/// the main thread.
#[tauri::command]
pub async fn note_page_drop(
    review: State<'_, Review>,
    path: String,
    index: usize,
) -> Result<(), String> {
    let review = review.inner().clone();
    tauri::async_runtime::spawn_blocking(move || review.drop_page(&path, index))
        .await
        .map_err(|e| e.to_string())
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

/// Opens one of the About links or the setup's links in the browser. No other address opens.
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

/// What the import commands refuse without a backup folder in the setup record.
pub const NO_BACKUP_DIR: &str = "Choose the backup folder first.";
/// What the app data commands refuse without a `.snbak` file (and without the empty choice).
pub const NO_SNBAK: &str = "Choose the .snbak file first.";

/// Backs the Supernote at `url` up into `dir` (Make a backup, or Resume with `resume`). The
/// folder is checked first; progress and the end come as events. Off the main thread: the
/// check reads the folder.
#[tauri::command]
pub async fn backup_start(
    jobs: State<'_, ImportJobs>,
    url: String,
    dir: PathBuf,
    resume: bool,
) -> Result<(), String> {
    let jobs = jobs.inner().clone();
    let dir = expand_home(&dir)?;
    tauri::async_runtime::spawn_blocking(move || jobs.backup(&url, &dir, resume))
        .await
        .map_err(|e| e.to_string())?
}

/// Stops the running backup; Resume continues it. Returns when it is gone.
#[tauri::command]
pub async fn backup_pause(jobs: State<'_, ImportJobs>) -> Result<(), String> {
    let jobs = jobs.inner().clone();
    tauri::async_runtime::spawn_blocking(move || jobs.pause())
        .await
        .map_err(|e| e.to_string())
}

/// What a backup folder holds, off the main thread.
#[tauri::command]
pub async fn backup_folder(dir: PathBuf) -> Result<BackupFolder, String> {
    let dir = expand_home(&dir)?;
    tauri::async_runtime::spawn_blocking(move || import_job::backup_folder_state(&dir))
        .await
        .map_err(|e| e.to_string())
}

/// The backup folder of the setup record.
fn backup_dir(config: &ConfigState) -> Result<PathBuf, String> {
    config
        .current()
        .setup
        .backup_dir
        .ok_or_else(|| NO_BACKUP_DIR.to_owned())
}

/// The `.snbak` of the setup record, or none for the empty choice.
fn snbak(config: &ConfigState, empty: bool) -> Result<Option<PathBuf>, String> {
    if empty {
        return Ok(None);
    }
    config
        .current()
        .setup
        .snbak
        .map(Some)
        .ok_or_else(|| NO_SNBAK.to_owned())
}

/// Checks the import of the backup folder of the setup record (`cloud import --dry-run`).
#[tauri::command]
pub async fn import_dry_run(
    config: State<'_, ConfigState>,
    jobs: State<'_, ImportJobs>,
    replace: bool,
    allow_empty: bool,
) -> Result<(), String> {
    let dir = backup_dir(&config)?;
    let jobs = jobs.inner().clone();
    tauri::async_runtime::spawn_blocking(move || jobs.import_dry_run(&dir, replace, allow_empty))
        .await
        .map_err(|e| e.to_string())?
}

/// Imports the backup folder of the setup record, with the app's server stopped. Off the main
/// thread: the stop can take the children's grace.
#[tauri::command]
pub async fn import_run(
    config: State<'_, ConfigState>,
    jobs: State<'_, ImportJobs>,
    replace: bool,
    allow_empty: bool,
) -> Result<(), String> {
    let dir = backup_dir(&config)?;
    let jobs = jobs.inner().clone();
    tauri::async_runtime::spawn_blocking(move || jobs.import(&dir, replace, allow_empty))
        .await
        .map_err(|e| e.to_string())?
}

/// Checks the import of the app data: the `.snbak` of the setup record, or none with `empty`.
#[tauri::command]
pub async fn app_data_dry_run(
    config: State<'_, ConfigState>,
    jobs: State<'_, ImportJobs>,
    empty: bool,
    replace: bool,
) -> Result<(), String> {
    let snbak = snbak(&config, empty)?;
    let jobs = jobs.inner().clone();
    tauri::async_runtime::spawn_blocking(move || jobs.app_data_dry_run(snbak.as_deref(), replace))
        .await
        .map_err(|e| e.to_string())?
}

/// Imports the app data, with the app's server stopped; then the sync mark is taken.
#[tauri::command]
pub async fn app_data_run(
    config: State<'_, ConfigState>,
    jobs: State<'_, ImportJobs>,
    empty: bool,
    replace: bool,
) -> Result<(), String> {
    let snbak = snbak(&config, empty)?;
    let jobs = jobs.inner().clone();
    tauri::async_runtime::spawn_blocking(move || jobs.app_data(snbak.as_deref(), replace))
        .await
        .map_err(|e| e.to_string())?
}

/// The step that runs, if any.
#[tauri::command]
pub fn job_state(jobs: State<'_, ImportJobs>) -> Option<JobKind> {
    jobs.state()
}

/// Saves the backup address, the backup folder, the `.snbak` file or that the deletes of the
/// first sync were seen, off the main thread.
#[tauri::command]
pub async fn set_setup_record(
    config: State<'_, ConfigState>,
    change: config::SetupChange,
) -> Result<AppConfig, String> {
    let state = config.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.set_setup_record(change))
        .await
        .map_err(|e| e.to_string())?
}

/// When a `.snbak` file was last written, in milliseconds since 1970.
#[tauri::command]
pub async fn snbak_time(path: PathBuf) -> Result<u64, String> {
    let path = expand_home(&path)?;
    tauri::async_runtime::spawn_blocking(move || file_time_ms(&path))
        .await
        .map_err(|e| e.to_string())?
}

/// The modification time of `path` in milliseconds since 1970.
pub fn file_time_ms(path: &Path) -> Result<u64, String> {
    let modified = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map_err(|e| format!("{} cannot be read: {e}", path.display()))?;
    let ms = modified
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| format!("{} has a time before 1970", path.display()))?
        .as_millis();
    Ok(u64::try_from(ms).unwrap_or(u64::MAX))
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
    fn the_markdown_folder_is_saved_for_the_account() {
        let dir = tempfile::tempdir().unwrap();
        let state = ConfigState::load(dir.path().to_owned());
        state
            .set_mode(Mode::Local, Some("/data/notes".into()), None)
            .unwrap();
        // It belongs to the account: none, no folder.
        assert_eq!(
            state.set_markdown_dir("/md".into()).unwrap_err(),
            NEEDS_ACCOUNT
        );
        state
            .try_update(|c| {
                c.account = Some("jordi".into());
                Ok::<(), ()>(())
            })
            .ok()
            .unwrap();
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let cfg = state.set_markdown_dir("~/Notes/md/".into()).unwrap();
        assert_eq!(cfg.markdown_dir(), Some(home.join("Notes/md")));
        let cfg = state.set_markdown_dir("/data/notes/../md".into()).unwrap();
        assert_eq!(cfg.markdown_dirs["jordi"], PathBuf::from("/data/md"));
        assert_eq!(
            config::load(dir.path()).markdown_dir(),
            Some(PathBuf::from("/data/md"))
        );
        // A folder next to the data folder with a longer name is outside it.
        state.set_markdown_dir("/data/notes-md".into()).unwrap();
    }

    #[test]
    fn the_seeded_markdown_folder_is_saved() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let state = ConfigState::load(dir.path().join("config"));
        state
            .set_mode(Mode::Local, Some(data.clone()), None)
            .unwrap();
        state
            .try_update(|c| {
                c.account = Some("jordi".into());
                Ok::<(), ()>(())
            })
            .ok()
            .unwrap();
        // Nothing in the pipeline's config: the default, and nothing is written for it.
        let saved = std::fs::read_to_string(dir.path().join("config/config.json")).unwrap();
        state.seed_markdown_dir().unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config/config.json")).unwrap(),
            saved
        );
        assert_eq!(
            state.current().markdown_dir(),
            Some(config::default_markdown_dir())
        );
        // The pipeline's entry is kept in the app config, so the OCR page shows it.
        std::fs::write(
            data.join("server.toml"),
            "[pipeline.markdown_dirs]\njordi = \"/notes/md\"\n",
        )
        .unwrap();
        state.seed_markdown_dir().unwrap();
        assert_eq!(
            config::load(&dir.path().join("config")).markdown_dirs["jordi"],
            PathBuf::from("/notes/md")
        );
        // The app's entry wins from then on.
        state.set_markdown_dir("/chosen/md".into()).unwrap();
        state.seed_markdown_dir().unwrap();
        assert_eq!(
            state.current().markdown_dir(),
            Some(PathBuf::from("/chosen/md"))
        );
    }

    #[test]
    fn set_mode_seeds_the_markdown_folder_of_a_kept_account() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let state = ConfigState::load(dir.path().join("config"));
        state
            .set_mode(Mode::Local, Some(data.clone()), None)
            .unwrap();
        state
            .try_update(|c| {
                c.account = Some("jordi".into());
                Ok::<(), ()>(())
            })
            .ok()
            .unwrap();
        // The pipeline's config gets an entry; Set with the same server keeps the account.
        std::fs::write(
            data.join("server.toml"),
            "[pipeline.markdown_dirs]\njordi = \"/notes/md\"\n",
        )
        .unwrap();
        let cfg = state
            .set_mode(Mode::Local, Some(data.clone()), None)
            .unwrap();
        assert_eq!(cfg.account.as_deref(), Some("jordi"));
        assert_eq!(cfg.markdown_dir(), Some(PathBuf::from("/notes/md")));
        assert_eq!(
            config::load(&dir.path().join("config")).markdown_dirs["jordi"],
            PathBuf::from("/notes/md")
        );
    }

    #[test]
    fn show_in_finder_makes_the_markdown_folder_first() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Documents").join("Markdown");
        let command = finder_command(&folder).unwrap();
        assert!(folder.is_dir(), "the folder was not made");
        assert_eq!(command.get_program(), "/usr/bin/open");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, [std::ffi::OsStr::new("--"), folder.as_os_str()]);
        // A folder that is there already is fine.
        finder_command(&folder).unwrap();
        // A file in the way is an error, said plainly.
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        let err = finder_command(&file.join("md")).unwrap_err();
        assert!(err.starts_with("The folder could not be made:"), "{err}");
    }

    #[test]
    fn a_markdown_folder_inside_the_data_folder_is_refused_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("data").join("md")).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let state = ConfigState::load(dir.path().join("config"));
        let logged_in = |data: &Path| {
            state
                .set_mode(Mode::Local, Some(data.to_owned()), None)
                .unwrap();
            state
                .try_update(|c| {
                    c.account = Some("jordi".into());
                    Ok::<(), ()>(())
                })
                .ok()
                .unwrap();
        };
        // The data folder through the link, the choice by its real path, and the other way.
        logged_in(&link.join("data"));
        for inside in [real.join("data").join("md"), real.join("data").join("new")] {
            assert_eq!(
                state.set_markdown_dir(inside.clone()).unwrap_err(),
                OUTSIDE_DATA,
                "{inside:?}"
            );
        }
        logged_in(&real.join("data"));
        for inside in [link.join("data").join("md"), link.join("data/new/deeper")] {
            assert_eq!(
                state.set_markdown_dir(inside.clone()).unwrap_err(),
                OUTSIDE_DATA,
                "{inside:?}"
            );
        }
        // Next to it is fine, through the link too.
        state.set_markdown_dir(link.join("md")).unwrap();
    }

    #[test]
    fn a_markdown_folder_inside_the_data_folder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let state = ConfigState::load(dir.path().to_owned());
        state
            .set_mode(Mode::Local, Some("/data/notes".into()), None)
            .unwrap();
        state
            .try_update(|c| {
                c.account = Some("jordi".into());
                Ok::<(), ()>(())
            })
            .ok()
            .unwrap();
        for inside in [
            "/data/notes",
            "/data/notes/",
            "/data/notes/md",
            "/data/./notes/x/../md",
        ] {
            assert_eq!(
                state.set_markdown_dir(inside.into()).unwrap_err(),
                OUTSIDE_DATA,
                "{inside}"
            );
        }
        assert_eq!(
            state.set_markdown_dir("md".into()).unwrap_err(),
            NEEDS_FULL_PATH
        );
        assert!(state.current().markdown_dirs.is_empty());
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
                crate::library::idle(),
                mode,
                Some(dir.into()),
                None,
                restart(&state),
                &ServeInfo::default(),
                || {},
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
                crate::library::idle(),
                Mode::Remote,
                None,
                None,
                restart(&state),
                &ServeInfo::default(),
                || {},
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
            data_dir: dir.path().join("data"),
            ..AppConfig::default()
        };
        config::save(dir.path(), &config).unwrap();
        let state = ConfigState::load(dir.path().to_path_buf());
        let store = MemoryStore::new();
        store.set("local|jordi", &Token::new("secret")).unwrap();
        store.set("local|other", &Token::new("other")).unwrap();
        let gate = LoginGate::default();
        // No server answers there: the revoke fails and the log out still happens.
        let serve_info = ServeInfo {
            // Port 1: no test binds it (macOS would allow it), so the connection is refused.
            listen: vec!["127.0.0.1:1".parse().unwrap()],
            ..ServeInfo::default()
        };
        let after =
            log_out_of(&gate, &store, &state, &crate::library::idle(), &serve_info).unwrap();
        assert_eq!(store.get("local|jordi").unwrap(), None);
        assert!(store.get("local|other").unwrap().is_some());
        assert_eq!(after.account, None);
        assert_eq!(after.email, None);
        assert_eq!(after.mode, Mode::Local);
        assert_eq!(state.current(), after);
        assert_eq!(config::load(dir.path()), after);
        // Without an account there is nothing to delete.
        log_out_of(&gate, &store, &state, &crate::library::idle(), &serve_info).unwrap();
    }

    #[test]
    fn only_the_about_and_setup_links_open() {
        assert!(allowed_link("https://ko-fi.com/V7V31T6CL9"));
        assert!(allowed_link("https://github.com/jordiboehme/notedown"));
        for link in SETUP_LINKS {
            assert!(allowed_link(link), "{link}");
        }
        assert!(!allowed_link(
            "https://github.com/jordiboehme/notedown/blob/main/docs/other.md"
        ));
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

    #[test]
    fn the_first_local_mode_turns_autostart_on_once() {
        let dir = tempfile::tempdir().unwrap();
        let state = ConfigState::load(dir.path().to_owned());
        let turned_on = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let set = |mode, url: Option<&str>| {
            let turned_on = turned_on.clone();
            tauri::async_runtime::block_on(set_mode_then(
                &state,
                Arc::new(LoginGate::default()),
                Arc::new(crate::token::MemoryStore::new()),
                crate::library::idle(),
                mode,
                Some("/data/a".into()),
                url.map(str::to_owned),
                || {},
                &ServeInfo::default(),
                move || {
                    turned_on.fetch_add(1, Ordering::SeqCst);
                },
            ))
            .unwrap()
            .config
        };
        let count = || turned_on.load(Ordering::SeqCst);
        // Another machine is no reason to start at login.
        let config = set(Mode::Remote, Some("https://n.example.org"));
        assert!(!config.setup.autostart_offered);
        assert_eq!(count(), 0);
        let config = set(Mode::Local, None);
        assert!(config.setup.autostart_offered);
        assert!(config::load(dir.path()).setup.autostart_offered);
        assert_eq!(count(), 1);
        // Never again, also when General turned it off and the mode changes back and forth.
        set(Mode::Remote, Some("https://n.example.org"));
        set(Mode::Local, None);
        assert_eq!(count(), 1);
    }

    #[test]
    fn the_mark_is_kept_with_its_key() {
        use config::{SyncCheck, SyncMark};
        let dir = tempfile::tempdir().unwrap();
        let state = ConfigState::load(dir.path().to_owned());
        state
            .set_mode(Mode::Local, Some("/data/a".into()), None)
            .unwrap();
        let log_in = |slug: &str| {
            state
                .try_update(|c| {
                    c.account = Some(slug.to_owned());
                    Ok::<(), ()>(())
                })
                .ok()
                .unwrap();
        };
        let mark = |key: &str, seq| SyncMark {
            key: key.to_owned(),
            seq,
        };
        let checked = |state: &ConfigState| {
            state
                .try_update(|c| {
                    c.setup.sync_checked = Some(SyncCheck::default());
                    c.setup.sync_acknowledged = true;
                    Ok::<(), ()>(())
                })
                .ok()
                .unwrap();
        };
        // No login: no mark.
        assert_eq!(state.take_sync_mark(mark("local|jordi", 7)), Ok(false));
        log_in("jordi");
        // A mark of another account is not taken.
        assert_eq!(state.take_sync_mark(mark("local|other", 7)), Ok(false));
        assert_eq!(state.take_sync_mark(mark("local|jordi", 7)), Ok(true));
        checked(&state);
        // The poller takes no second one for the same account.
        assert_eq!(state.take_sync_mark(mark("local|jordi", 9)), Ok(false));
        // Log out and log in again: the mark and its check stay.
        state.forget_login().unwrap();
        assert_eq!(
            state.current().setup.sync_mark,
            Some(mark("local|jordi", 7))
        );
        log_in("jordi");
        let setup = state.current().setup;
        assert_eq!(setup.sync_mark, Some(mark("local|jordi", 7)));
        assert!(setup.sync_checked.is_some() && setup.sync_acknowledged);
        // Another server keeps it too, but it is not that account's.
        state
            .set_mode(Mode::Local, Some("/data/b".into()), None)
            .unwrap();
        assert_eq!(
            state.current().setup.sync_mark,
            Some(mark("local|jordi", 7))
        );
        // A mark of another key replaces it, and the old check goes with it.
        log_in("anna");
        assert_eq!(state.take_sync_mark(mark("local|anna", 3)), Ok(true));
        let setup = state.current().setup;
        assert_eq!(setup.sync_mark, Some(mark("local|anna", 3)));
        assert_eq!((setup.sync_checked, setup.sync_acknowledged), (None, false));
        // A new import of the same account replaces its mark.
        checked(&state);
        assert_eq!(state.set_sync_mark(mark("local|anna", 20)), Ok(true));
        let setup = config::load(dir.path()).setup;
        assert_eq!(setup.sync_mark, Some(mark("local|anna", 20)));
        assert_eq!((setup.sync_checked, setup.sync_acknowledged), (None, false));
        // Never for an account that is not logged in.
        assert_eq!(state.set_sync_mark(mark("local|jordi", 30)), Ok(false));
    }

    #[test]
    fn the_setup_record_takes_only_what_the_window_sends() {
        let dir = tempfile::tempdir().unwrap();
        let state = ConfigState::load(dir.path().to_owned());
        let change = |json: &str| {
            state
                .set_setup_record(serde_json::from_str(json).unwrap())
                .map(|c| c.setup)
        };
        let setup = change(
            r#"{"backupUrl":" http://192.168.1.20:8089 ","backupDir":"~/supernote-backup-2026-11-17","snbak":"/x/a.snbak"}"#,
        )
        .unwrap();
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(
            setup.backup_url.as_deref(),
            Some("http://192.168.1.20:8089")
        );
        assert_eq!(
            setup.backup_dir,
            Some(home.join("supernote-backup-2026-11-17"))
        );
        assert_eq!(setup.snbak, Some(PathBuf::from("/x/a.snbak")));
        // A field that is left out stays; null clears it.
        let setup = change(r#"{"snbak":null}"#).unwrap();
        assert_eq!(setup.snbak, None);
        assert!(setup.backup_dir.is_some() && setup.backup_url.is_some());
        // A folder must be a full path.
        assert_eq!(
            change(r#"{"backupDir":"backup"}"#).unwrap_err(),
            NEEDS_FULL_PATH
        );
        // The window cannot set what Rust keeps.
        assert!(serde_json::from_str::<config::SetupChange>(r#"{"syncMark":null}"#).is_err());
        assert!(serde_json::from_str::<config::SetupChange>(r#"{"setupShown":true}"#).is_err());
        assert_eq!(config::load(dir.path()).setup, state.current().setup);
    }

    #[test]
    fn the_snbak_time_is_the_file_time() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.snbak");
        std::fs::write(&file, "x").unwrap();
        let when = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_763_300_000_123);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(when)
            .unwrap();
        assert_eq!(file_time_ms(&file), Ok(1_763_300_000_123));
        assert!(file_time_ms(&dir.path().join("missing.snbak")).is_err());
    }
}
