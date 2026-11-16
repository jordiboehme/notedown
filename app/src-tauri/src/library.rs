//! The library: the files of the account on the server, as the library window shows them.
//!
//! A [`Library`] is bound to one server address, one account and one token. The token is read
//! from the Keychain once, when the library binds, and kept here; it is dropped on a 401. When
//! the address, the account or the token changes (a login, a log out, a new place of the
//! server, a new address of the app's own server), the library binds again: the jobs and the
//! follower of the old binding are cancelled, the cache is dropped, and the window gets a
//! `state` event followed by `relist`.
//!
//! A refused token stays in the Keychain until the next login. So after the key changes back
//! (the same server and account again) or after a restart of the app, the library shows Ready
//! until the next call answers 401.
//!
//! Every event of the library is a [`LibraryEvent`]; none of them holds the token.
//!
//! The listed folders and how they follow the server's changes: `library/cache.rs` (the rules)
//! and `library/listing.rs` (the calls and the follower).

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use crate::config::AppConfig;
use crate::poller::rest_base;
use crate::rest::{self, Rest, RestError, Sleeper};
use crate::serve_info::ServeInfo;
use crate::supervisor::Clock;
use crate::token::{TokenStore, token_key};

/// The event that carries every [`LibraryEvent`] to the library window.
pub const LIBRARY_EVENT: &str = "library";

/// What the library window can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LibraryState {
    /// No login for the account in the config, or no account.
    LoggedOut,
    /// No mode, or Remote mode without an address.
    NotSetUp,
    /// The server does not answer.
    Unreachable,
    /// 423 `not_imported`: the Supernote has not been imported yet.
    NotImported,
    /// 401: the server refused the stored login.
    Refused,
    /// The library can be listed.
    Ready,
}

impl LibraryState {
    /// What the window says in this state; nothing when it is ready.
    pub fn text(self) -> Option<&'static str> {
        match self {
            Self::NotSetUp => Some("Choose where the server runs in Settings."),
            Self::LoggedOut => Some("Log in to see your library."),
            Self::Unreachable => Some("The server does not answer."),
            Self::NotImported => Some("The Supernote has not been imported yet."),
            Self::Refused => Some("The server refused the login. Log in again."),
            Self::Ready => None,
        }
    }

    /// The Settings page the window's button opens in this state (a page of
    /// `app/src/settings/pages.ts`); none when it is ready.
    pub fn settings_page(self) -> Option<&'static str> {
        match self {
            Self::NotSetUp | Self::Unreachable => Some("server"),
            Self::LoggedOut | Self::Refused => Some("account"),
            Self::NotImported => Some("supernote"),
            Self::Ready => None,
        }
    }
}

/// The text of a binding whose Keychain read failed (state `loggedOut`, page "account").
pub const KEYCHAIN_UNREADABLE: &str = "The Keychain could not be read.";
/// The text of a binding whose client could not be made (state `unreachable`, page "server").
pub const NO_CLIENT: &str = "The app could not set up the connection.";

/// The state with its text and the Settings page its button opens: the `state` event and the
/// answer of `library_state`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StateView {
    pub state: LibraryState,
    pub text: Option<String>,
    pub page: Option<String>,
}

impl StateView {
    pub fn of(state: LibraryState) -> Self {
        Self {
            state,
            text: state.text().map(str::to_owned),
            page: state.settings_page().map(str::to_owned),
        }
    }

    /// `state` with another text than its own.
    pub fn with_text(state: LibraryState, text: &str) -> Self {
        Self {
            text: Some(text.to_owned()),
            ..Self::of(state)
        }
    }
}

/// A file or a folder of the library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: u64,
    pub path: String,
    pub name: String,
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub md5: Option<String>,
    pub mtime_ms: i64,
    pub writable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EntryKind {
    File,
    Folder,
}

/// A listed folder: the entries loaded so far.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderState {
    pub path: String,
    pub entries: Vec<Entry>,
    pub complete: bool,
    pub loading: bool,
    pub error: Option<String>,
}

/// An upload, a download or a change, as the window shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: u64,
    pub kind: JobKind,
    pub label: String,
    pub done: u64,
    pub total: Option<u64>,
    pub state: JobState,
    pub text: Option<String>,
    pub question: Option<Clash>,
    pub result: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum JobKind {
    Upload,
    Download,
    Change,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum JobState {
    Queued,
    Running,
    Waiting,
    Asking,
    Done,
    Failed,
    Cancelled,
}

/// A name that is taken where a job wants to put something.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Clash {
    pub name: String,
    pub existing: String,
    pub can_replace: bool,
}

/// Every event the library sends to its window, as `{ "type": ..., ... }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum LibraryEvent {
    /// The state, its text or its Settings page changed. A binding sends it, then `Relist`; a
    /// call's answer (unreachable, not imported, refused, ready again) sends it alone.
    State(StateView),
    /// A folder was listed or changed.
    Folder { folder: FolderState },
    /// A job moved on.
    Job { job: Job },
    /// The library bound again (or must list again): the window drops what it shows and lists
    /// its folders again.
    Relist,
}

/// Where the library's events go. It must not block: the app only puts them into a channel.
pub type Emit = Arc<dyn Fn(LibraryEvent) + Send + Sync>;

/// The config and the start lines of the app's own server, as they are now.
pub type PlaceFn = Arc<dyn Fn() -> (AppConfig, ServeInfo) + Send + Sync>;

/// Why a library call did not run or failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryError {
    /// The library has no login to call with: not set up, logged out or refused.
    NotReady(LibraryState),
    /// The server answered with an error, or did not answer.
    Rest(RestError),
    /// The library bound again while the call ran: its answer belongs to the old binding.
    Changed,
}

impl fmt::Display for LibraryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Ready without a client: a call that met a binding half way.
            Self::NotReady(state) => f.write_str(
                state
                    .text()
                    .unwrap_or("The library is starting again. Try again in a moment."),
            ),
            Self::Rest(e) => e.fmt(f),
            Self::Changed => f.write_str("The library changed. List again."),
        }
    }
}

impl std::error::Error for LibraryError {}

/// One binding's client, for calls made outside the library's lock. Cancelled when the library
/// binds again or the token is dropped: a job or the follower that holds it then ends.
#[derive(Clone)]
pub struct Session {
    generation: u64,
    rest: Arc<Rest>,
    cancelled: Arc<AtomicBool>,
}

impl Session {
    pub fn rest(&self) -> &Rest {
        &self.rest
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The library bound again, or dropped its token, since this session began.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// The library of the account in the config. Cheap to clone; every clone is the same library.
#[derive(Clone)]
pub struct Library {
    inner: Arc<Inner>,
}

struct Inner {
    store: Arc<dyn TokenStore>,
    place: PlaceFn,
    emit: Emit,
    clock: Clock,
    sleeper: Sleeper,
    /// One binding at a time, held across the Keychain read; the place is read under it, so
    /// the newest place always wins.
    rebinding: Mutex<()>,
    /// Held only for short reads and swaps, never across a Keychain read or a call.
    bound: Mutex<Bound>,
    /// The cache worker: the listed folders. A page merge or a batch of events runs whole under
    /// this lock, in the order they take it; no call is made under it. Never taken together
    /// with `bound`.
    cache: Mutex<cache::Cache>,
    /// Whether the window is visible and when the follower last read the events.
    follow: Mutex<listing::Follow>,
    /// One follow step at a time.
    stepping: Mutex<()>,
    /// Wakes the follower's thread, once it runs.
    waker: Mutex<Option<Sender<()>>>,
}

/// What the library is bound to.
struct Bound {
    /// The REST base and the Keychain key it was bound for; `None` before the first binding.
    place: Option<(Option<String>, Option<String>)>,
    generation: u64,
    /// The client with the token; `None` without a token.
    rest: Option<Arc<Rest>>,
    cancelled: Arc<AtomicBool>,
    state: LibraryState,
    /// What the window is told; `None` before the first binding.
    view: Option<StateView>,
    /// The server refused the token of this binding; it was dropped.
    refused: bool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Library {
    /// A library with the monotonic clock and real sleeps. It binds at the first
    /// [`refresh`](Self::refresh).
    pub fn new(store: Arc<dyn TokenStore>, place: PlaceFn, emit: Emit) -> Self {
        Self::with_timing(
            store,
            place,
            emit,
            rest::monotonic_clock(),
            rest::real_sleeper(),
        )
    }

    /// [`new`](Self::new) with another clock and sleeper for the client.
    pub fn with_timing(
        store: Arc<dyn TokenStore>,
        place: PlaceFn,
        emit: Emit,
        clock: Clock,
        sleeper: Sleeper,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                place,
                emit,
                clock,
                sleeper,
                rebinding: Mutex::new(()),
                bound: Mutex::new(Bound {
                    place: None,
                    generation: 0,
                    rest: None,
                    cancelled: Arc::new(AtomicBool::new(false)),
                    state: LibraryState::NotSetUp,
                    view: None,
                    refused: false,
                }),
                cache: Mutex::new(cache::Cache::default()),
                follow: Mutex::new(listing::Follow::default()),
                stepping: Mutex::new(()),
                waker: Mutex::new(None),
            }),
        }
    }

    /// The state as it is now; `None` before the first binding. It waits for no call and no
    /// Keychain read.
    pub fn state(&self) -> Option<StateView> {
        lock(&self.inner.bound).view.clone()
    }

    /// Counts the bindings.
    pub fn generation(&self) -> u64 {
        lock(&self.inner.bound).generation
    }

    /// Binds again when the REST base or the account's Keychain key changed since the last
    /// binding (a new place, a new address of the app's own server). A login the server
    /// refused stays refused for the same key. Blocking: run it off the main thread.
    pub fn refresh(&self) {
        self.bind(false);
    }

    /// Binds again and reads the token again, also for the same base and key: after a login
    /// and a log out. Blocking: run it off the main thread.
    pub fn relogin(&self) {
        self.bind(true);
    }

    fn bind(&self, again: bool) {
        let inner = &*self.inner;
        let _one_at_a_time = lock(&inner.rebinding);
        let (config, serve_info) = (inner.place)();
        let base = rest_base(&config, &serve_info);
        let key = token_key(&config);
        let place = Some((base.clone(), key.clone()));
        // Stop what the old binding does before the Keychain is read.
        let (old, still_refused, generation) = {
            let mut bound = lock(&inner.bound);
            if !again && bound.place == place {
                return;
            }
            let same_key = bound.place.as_ref().is_some_and(|(_, k)| *k == key);
            let still_refused = !again && bound.refused && same_key;
            bound.cancelled.store(true, Ordering::SeqCst);
            bound.cancelled = Arc::new(AtomicBool::new(false));
            bound.generation += 1;
            bound.place = place;
            (bound.rest.take(), still_refused, bound.generation)
        };
        // The client goes outside the lock: its end waits for its thread.
        drop(old);
        // The follower of the old binding stops (its session is cancelled) and the cache goes.
        self.reset_listing(generation);
        // A binding that failed on this Mac (the Keychain, the client) records no place: the
        // next nudge tries again.
        let (view, rest, failed) = match (base, key) {
            (None, _) => (StateView::of(LibraryState::NotSetUp), None, false),
            (Some(_), None) => (StateView::of(LibraryState::LoggedOut), None, false),
            (Some(_), Some(_)) if still_refused => {
                (StateView::of(LibraryState::Refused), None, false)
            }
            (Some(base), Some(key)) => match inner.store.get(&key) {
                Ok(Some(token)) => match Rest::with_timing(
                    &base,
                    Some(token),
                    inner.clock.clone(),
                    inner.sleeper.clone(),
                ) {
                    Ok(rest) => (
                        StateView::of(LibraryState::Ready),
                        Some(Arc::new(rest)),
                        false,
                    ),
                    Err(_) => (
                        StateView::with_text(LibraryState::Unreachable, NO_CLIENT),
                        None,
                        true,
                    ),
                },
                Ok(None) => (StateView::of(LibraryState::LoggedOut), None, false),
                Err(_) => (
                    StateView::with_text(LibraryState::LoggedOut, KEYCHAIN_UNREADABLE),
                    None,
                    true,
                ),
            },
        };
        let mut bound = lock(&inner.bound);
        if failed {
            bound.place = None;
        }
        bound.rest = rest;
        bound.state = view.state;
        bound.refused = view.state == LibraryState::Refused;
        bound.view = Some(view.clone());
        // Under the same guard: no settled state comes between the two.
        (inner.emit)(LibraryEvent::State(view));
        (inner.emit)(LibraryEvent::Relist);
    }

    /// The client of the binding, for a call made outside the library.
    pub fn session(&self) -> Result<Session, LibraryError> {
        let bound = lock(&self.inner.bound);
        match &bound.rest {
            Some(rest) => Ok(Session {
                generation: bound.generation,
                rest: rest.clone(),
                cancelled: bound.cancelled.clone(),
            }),
            None => Err(LibraryError::NotReady(bound.state)),
        }
    }

    /// Takes the outcome of a call made with `session` into the state: a 401 drops the token,
    /// no answer is unreachable, 423 `not_imported` says so, and a success is ready again. An
    /// outcome of an earlier binding changes nothing.
    pub fn settle<T>(&self, session: &Session, result: &Result<T, RestError>) {
        let mut bound = lock(&self.inner.bound);
        if bound.generation != session.generation || bound.rest.is_none() {
            return;
        }
        let next = match result {
            Ok(_) => LibraryState::Ready,
            Err(RestError::TokenRefused) => LibraryState::Refused,
            Err(RestError::Unreachable) => LibraryState::Unreachable,
            Err(RestError::NotImported) => LibraryState::NotImported,
            Err(_) => return,
        };
        let dropped = if next == LibraryState::Refused {
            bound.refused = true;
            bound.cancelled.store(true, Ordering::SeqCst);
            bound.rest.take()
        } else {
            None
        };
        let changed = bound.state != next;
        bound.state = next;
        bound.view = Some(StateView::of(next));
        if changed {
            (self.inner.emit)(LibraryEvent::State(StateView::of(next)));
        }
        drop(bound);
        drop(dropped);
    }

    /// One call with the binding's client, settled. Blocking: run it off the main thread.
    pub fn call<T>(
        &self,
        call: impl FnOnce(&Rest) -> Result<T, RestError>,
    ) -> Result<T, LibraryError> {
        let session = self.session()?;
        let result = call(session.rest());
        self.settle(&session, &result);
        result.map_err(LibraryError::Rest)
    }
}

mod cache;
mod listing;

pub use cache::{Key, Listed};
pub use listing::{FOLLOW_EVERY, RecycleItem, RecyclePage};

/// A library that is never set up, for tests of other parts that need one.
#[cfg(test)]
pub(crate) fn idle() -> Library {
    Library::new(
        Arc::new(crate::token::MemoryStore::new()),
        Arc::new(|| (AppConfig::default(), ServeInfo::default())),
        Arc::new(|_| {}),
    )
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod listing_tests;
