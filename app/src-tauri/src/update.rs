//! The self-updater. Rust checks GitHub's `latest.json` at start and then once a day; a found
//! update opens the settings window on the About page, which shows its version and notes and
//! asks. The install downloads and installs the update (the plugin checks its signature), then
//! the restart stops the server and the pipeline before the new version starts
//! ([`crate::quit::QuitFlow::restart_with`]). The tray item "Check for Updates..." and the About
//! page check at once.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::Serialize;

/// The event that carries a new [`UpdateState`] to the settings window.
pub const UPDATE_STATE_EVENT: &str = "update-state";

/// The time between two automatic checks.
pub const CHECK_EVERY_MS: u64 = 24 * 60 * 60 * 1000;

/// How often the automatic check looks whether a check is due. A Mac that sleeps does not
/// count the time asleep, so the check runs within this time after the 24 hours.
pub const WAKE_EVERY: Duration = Duration::from_secs(60 * 60);

/// What a check found: the new version and its release notes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub notes: Option<String>,
}

/// What the About page shows of the updater.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum UpdateState {
    /// Nothing checked yet in this run, or nothing to tell.
    Idle,
    Checking,
    /// A newer version is ready to install.
    Found {
        version: String,
        notes: Option<String>,
    },
    UpToDate,
    /// The check did not get an answer.
    Failed {
        reason: String,
    },
    Installing {
        version: String,
    },
    /// The download or the install failed; the running version stays.
    InstallFailed {
        version: String,
        reason: String,
    },
}

/// Whether the automatic check is due: never checked, 24 hours since the last check, or a
/// clock that went back (else it would wait until the clock catches up).
pub fn should_check(last_check_ms: Option<u64>, now_ms: u64) -> bool {
    match last_check_ms {
        None => true,
        Some(last) => last > now_ms || now_ms - last >= CHECK_EVERY_MS,
    }
}

/// What one check answers: the update found with its handle for the install, none, or why it
/// failed.
pub type Checked<P> = Result<Option<(UpdateInfo, P)>, String>;

/// Hears every new [`UpdateState`].
pub type Listener = Arc<dyn Fn(&UpdateState) + Send + Sync>;

/// The updater's state and the update found last. `P` is the plugin's update handle (a fake
/// in tests). One per app.
pub struct Updates<P> {
    inner: Mutex<Inner<P>>,
    listener: Listener,
}

struct Inner<P> {
    state: UpdateState,
    found: Option<(UpdateInfo, P)>,
}

impl<P: Clone> Updates<P> {
    pub fn new(listener: Listener) -> Self {
        Self {
            inner: Mutex::new(Inner {
                state: UpdateState::Idle,
                found: None,
            }),
            listener,
        }
    }

    pub fn state(&self) -> UpdateState {
        self.lock().state.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner<P>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sets the state and tells it, outside the lock.
    fn tell(&self, change: impl FnOnce(&mut Inner<P>) -> Option<UpdateState>) {
        let told = change(&mut self.lock());
        if let Some(state) = told {
            (self.listener)(&state);
        }
    }

    /// A check the user asked for: Checking, then the answer. Not while an install runs.
    pub fn check_now(
        &self,
        check: impl FnOnce() -> Checked<P>,
    ) -> Result<Option<UpdateInfo>, String> {
        self.check(check, false)
    }

    /// The automatic check: it shows only a found update, and a failure stays silent.
    pub fn check_quietly(
        &self,
        check: impl FnOnce() -> Checked<P>,
    ) -> Result<Option<UpdateInfo>, String> {
        self.check(check, true)
    }

    fn check(
        &self,
        check: impl FnOnce() -> Checked<P>,
        quiet: bool,
    ) -> Result<Option<UpdateInfo>, String> {
        let mut installing = false;
        self.tell(|inner| {
            installing = matches!(inner.state, UpdateState::Installing { .. });
            (!installing && !quiet).then(|| {
                inner.state = UpdateState::Checking;
                inner.state.clone()
            })
        });
        if installing {
            return Err("Notedown is installing an update".to_owned());
        }
        let answer = check();
        let result = answer
            .as_ref()
            .map(|found| found.as_ref().map(|(info, _)| info.clone()))
            .map_err(Clone::clone);
        self.tell(|inner| {
            let next = match answer {
                Ok(Some((info, handle))) => {
                    let state = UpdateState::Found {
                        version: info.version.clone(),
                        notes: info.notes.clone(),
                    };
                    inner.found = Some((info, handle));
                    state
                }
                Ok(None) => {
                    inner.found = None;
                    match (&inner.state, quiet) {
                        (_, false) => UpdateState::UpToDate,
                        // An update shown before is gone (a release taken back).
                        (UpdateState::Found { .. }, true) => UpdateState::Idle,
                        (_, true) => return None,
                    }
                }
                Err(reason) if !quiet => UpdateState::Failed { reason },
                Err(_) => return None,
            };
            inner.state = next.clone();
            Some(next)
        });
        result
    }

    /// The found update for the install, now Installing. `None` when nothing was found.
    pub fn start_install(&self) -> Option<(UpdateInfo, P)> {
        let mut taken = None;
        self.tell(|inner| {
            let (info, handle) = inner.found.clone()?;
            inner.state = UpdateState::Installing {
                version: info.version.clone(),
            };
            taken = Some((info, handle));
            Some(inner.state.clone())
        });
        taken
    }

    /// The install failed: the update stays found for another try.
    pub fn install_failed(&self, reason: &str) {
        self.tell(|inner| {
            let version = inner.found.as_ref()?.0.version.clone();
            inner.state = UpdateState::InstallFailed {
                version,
                reason: reason.to_owned(),
            };
            Some(inner.state.clone())
        });
    }
}

/// Asks GitHub's `latest.json` (the endpoint of tauri.conf.json) whether a newer version is
/// there; the plugin compares the versions. It blocks: run it off the main thread and off the
/// async runtime.
pub fn check_github(app: &tauri::AppHandle) -> Checked<tauri_plugin_updater::Update> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(|e| e.to_string())?;
    let found = tauri::async_runtime::block_on(updater.check()).map_err(|e| e.to_string())?;
    Ok(found.map(|update| {
        let info = UpdateInfo {
            version: update.version.clone(),
            notes: update.body.clone().filter(|notes| !notes.trim().is_empty()),
        };
        (info, update)
    }))
}

/// What a round of the automatic check did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Round {
    NotDue,
    Checked(Result<Option<UpdateInfo>, String>),
}

/// One round of the automatic check (at start, then every [`WAKE_EVERY`]): it checks only when
/// [`should_check`] says so, and saves the time of a check that got an answer, so a failed one
/// is tried again at the next round.
pub fn auto_round<P: Clone>(
    updates: &Updates<P>,
    last_check_ms: Option<u64>,
    now_ms: u64,
    save: impl FnOnce(u64),
    check: impl FnOnce() -> Checked<P>,
) -> Round {
    if !should_check(last_check_ms, now_ms) {
        return Round::NotDue;
    }
    let answer = updates.check_quietly(check);
    if answer.is_ok() {
        save(now_ms);
    }
    Round::Checked(answer)
}

#[cfg(test)]
mod tests;
