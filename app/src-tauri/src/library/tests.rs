//! The library's binding against a scripted server on loopback, with a store in memory (never
//! the real Keychain), a fake clock and a sleeper that does not sleep.

use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::commands::{ConfigState, change_place, log_in_to, log_out_of};
use crate::config::{self, Mode};
use crate::login::{LoginGate, Password};
use crate::rest::test_http::{Scripted, answer};
use crate::token::{MemoryStore, Token, TokenError};

pub(super) const TOKEN: &str = "tok-SECRET-4711";

/// A store in memory that counts its reads.
#[derive(Default)]
pub(super) struct CountingStore {
    inner: MemoryStore,
    gets: AtomicUsize,
}

impl CountingStore {
    pub(super) fn with(key: &str, token: &str) -> Arc<Self> {
        let store = Arc::new(Self::default());
        store.inner.set(key, &Token::new(token)).unwrap();
        store
    }

    fn gets(&self) -> usize {
        self.gets.load(Ordering::SeqCst)
    }
}

impl TokenStore for CountingStore {
    fn get(&self, key: &str) -> Result<Option<Token>, TokenError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        self.inner.get(key)
    }

    fn set(&self, key: &str, token: &Token) -> Result<(), TokenError> {
        self.inner.set(key, token)
    }

    fn delete(&self, key: &str) -> Result<(), TokenError> {
        self.inner.delete(key)
    }
}

/// The place the library reads, changed by the test.
#[derive(Clone)]
pub(super) struct Place(Arc<Mutex<(AppConfig, ServeInfo)>>);

impl Place {
    fn new(config: AppConfig) -> Self {
        Self(Arc::new(Mutex::new((config, ServeInfo::default()))))
    }

    /// Remote mode at the scripted server's port.
    pub(super) fn remote(port: u16, account: Option<&str>) -> Self {
        Self::new(AppConfig {
            mode: Mode::Remote,
            server_url: Some(format!("http://127.0.0.1:{port}")),
            account: account.map(str::to_owned),
            ..AppConfig::default()
        })
    }

    /// Local mode whose server listens on `port` (the start lines of `serve`).
    fn local(dir: &std::path::Path, port: u16, account: Option<&str>) -> Self {
        let place = Self::new(AppConfig {
            mode: Mode::Local,
            data_dir: dir.to_path_buf(),
            account: account.map(str::to_owned),
            ..AppConfig::default()
        });
        place.listen_on(port);
        place
    }

    fn listen_on(&self, port: u16) {
        self.0.lock().unwrap().1.listen = vec![format!("127.0.0.1:{port}").parse().unwrap()];
    }

    fn change(&self, change: impl FnOnce(&mut AppConfig)) {
        change(&mut self.0.lock().unwrap().0);
    }

    pub(super) fn func(&self) -> PlaceFn {
        let place = self.0.clone();
        Arc::new(move || place.lock().unwrap().clone())
    }
}

#[derive(Clone, Default)]
pub(super) struct Events(pub(super) Arc<Mutex<Vec<LibraryEvent>>>);

impl Events {
    pub(super) fn emit(&self) -> Emit {
        let events = self.0.clone();
        Arc::new(move |event| events.lock().unwrap().push(event))
    }

    pub(super) fn all(&self) -> Vec<LibraryEvent> {
        self.0.lock().unwrap().clone()
    }

    fn states(&self) -> Vec<LibraryState> {
        self.all()
            .into_iter()
            .filter_map(|e| match e {
                LibraryEvent::State(view) => Some(view.state),
                _ => None,
            })
            .collect()
    }
}

pub(super) fn no_sleep() -> Sleeper {
    Arc::new(|_: Duration| {})
}

fn fake_clock() -> Clock {
    Arc::new(|| 0)
}

fn make(store: Arc<dyn TokenStore>, place: PlaceFn) -> (Library, Events) {
    let events = Events::default();
    let library = Library::with_timing(store, place, events.emit(), fake_clock(), no_sleep());
    (library, events)
}

fn ok() -> String {
    answer("200 OK", "{}")
}

fn get_account(library: &Library) -> Result<Value, LibraryError> {
    library.call(|rest| rest.get_json::<Value>("/account", &[]))
}

pub(super) fn remote_key(port: u16, slug: &str) -> String {
    format!("http://127.0.0.1:{port}|{slug}")
}

fn state_event(state: LibraryState) -> LibraryEvent {
    LibraryEvent::State(StateView::of(state))
}

/// The `relist` of a binding; `leave` when the account changed.
fn relist(leave: bool) -> LibraryEvent {
    LibraryEvent::Relist { leave }
}

/// The `leave` of each `relist` sent so far.
fn leaves(events: &Events) -> Vec<bool> {
    events
        .all()
        .into_iter()
        .filter_map(|e| match e {
            LibraryEvent::Relist { leave } => Some(leave),
            _ => None,
        })
        .collect()
}

#[test]
fn the_library_is_bound_to_base_account_and_token() {
    let server = Scripted::start(vec![ok(), ok()]);
    let key = remote_key(server.port, "jordi");
    let store = CountingStore::with(&key, "tok-1");
    let place = Place::remote(server.port, Some("jordi"));
    let (library, events) = make(store.clone(), place.func());

    library.refresh();
    assert_eq!(library.state().unwrap(), StateView::of(LibraryState::Ready));
    // A binding says its state, then has the window list again.
    assert_eq!(
        events.all(),
        // The first binding leaves no review: there was no account before it.
        vec![state_event(LibraryState::Ready), relist(false)]
    );
    assert_eq!(store.gets(), 1);
    let first = library.session().unwrap();
    assert_eq!(first.rest().base(), server.base());
    get_account(&library).unwrap();
    assert_eq!(
        server.next().header("authorization"),
        Some("Bearer tok-1"),
        "the call uses the bound token"
    );

    // The same place again: no new binding (no event, the session goes on) and no Keychain
    // read.
    library.refresh();
    assert_eq!(store.gets(), 1);
    assert_eq!(events.all().len(), 2);
    assert!(!first.is_cancelled());

    // A new token of the same account (a login): read once more, the old binding ends.
    store.set(&key, &Token::new("tok-2")).unwrap();
    library.relogin();
    assert_eq!(store.gets(), 2);
    assert!(first.is_cancelled());
    // The same account: the review stays.
    assert_eq!(events.all().last(), Some(&relist(false)));
    assert_eq!(events.states(), [LibraryState::Ready, LibraryState::Ready]);
    get_account(&library).unwrap();
    assert_eq!(server.next().header("authorization"), Some("Bearer tok-2"));

    // Another account without a login: logged out, the binding ends.
    let second = library.session().unwrap();
    place.change(|c| c.account = Some("anna".into()));
    library.refresh();
    assert!(second.is_cancelled());
    assert_eq!(
        library.session().err(),
        Some(LibraryError::NotReady(LibraryState::LoggedOut))
    );
    assert_eq!(
        events.all()[events.all().len() - 2..],
        [state_event(LibraryState::LoggedOut), relist(true)]
    );

    // Another base with the same key: the app's own server on a new port.
    let dir = tempfile::tempdir().unwrap();
    let local = Place::local(dir.path(), 41001, Some("jordi"));
    let store = CountingStore::with("local|jordi", "tok-3");
    let (library, events) = make(store.clone(), local.func());
    library.refresh();
    let old = library.session().unwrap();
    assert_eq!(old.rest().base(), "http://127.0.0.1:41001/rest/v1");
    local.listen_on(41002);
    library.refresh();
    assert!(old.is_cancelled());
    assert_eq!(
        library.session().unwrap().rest().base(),
        "http://127.0.0.1:41002/rest/v1"
    );
    assert_eq!(store.gets(), 2);
    assert_eq!(events.states(), [LibraryState::Ready, LibraryState::Ready]);
    // The same key at a new address: the review stays.
    assert_eq!(leaves(&events), [false, false]);
}

/// A config in Local mode with the account `jordi`, saved in `dir`.
fn local_state(dir: &std::path::Path) -> ConfigState {
    let data = dir.join("data");
    let config = AppConfig {
        mode: Mode::Local,
        data_dir: data,
        account: Some("jordi".into()),
        email: Some("me@example.org".into()),
        ..AppConfig::default()
    };
    config::save(dir, &config).unwrap();
    ConfigState::load(dir.to_path_buf())
}

/// The app's own server as listening where nobody does: a revoke on log out fails at once
/// and never reaches a real server of this Mac.
fn nobody_listens() -> crate::serve_info::ServeInfo {
    crate::serve_info::ServeInfo {
        // Below 1024: no test listens there, so the connection is refused.
        listen: vec!["127.0.0.1:1".parse().unwrap()],
        ..crate::serve_info::ServeInfo::default()
    }
}

/// The library of a [`ConfigState`], as the app makes it.
fn library_of(state: &ConfigState, store: Arc<dyn TokenStore>) -> (Library, Events) {
    let reader = state.reader();
    make(
        store,
        Arc::new(move || (reader.current(), ServeInfo::default())),
    )
}

#[test]
fn log_out_stops_the_library() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_state(dir.path());
    let store = Arc::new(MemoryStore::new());
    store.set("local|jordi", &Token::new(TOKEN)).unwrap();
    let (library, events) = library_of(&state, store.clone());
    library.refresh();
    let session = library.session().unwrap();

    log_out_of(
        &LoginGate::default(),
        &*store,
        &state,
        &library,
        &nobody_listens(),
    )
    .unwrap();

    assert!(session.is_cancelled(), "the jobs of the login end");
    assert_eq!(
        library.state().unwrap(),
        StateView {
            state: LibraryState::LoggedOut,
            text: Some("Log in to see your library.".into()),
            page: Some("account".into()),
        }
    );
    assert_eq!(
        events.all(),
        vec![
            state_event(LibraryState::Ready),
            relist(false),
            state_event(LibraryState::LoggedOut),
            relist(true),
        ]
    );
    assert_eq!(
        library.session().err(),
        Some(LibraryError::NotReady(LibraryState::LoggedOut))
    );
    assert_eq!(
        get_account(&library).unwrap_err().to_string(),
        "Log in to see your library."
    );
}

#[test]
fn a_new_place_rebinds_the_library() {
    let dir = tempfile::tempdir().unwrap();
    let state = local_state(dir.path());
    let store = Arc::new(MemoryStore::new());
    store.set("local|jordi", &Token::new(TOKEN)).unwrap();
    let (library, events) = library_of(&state, store.clone());
    library.refresh();
    let session = library.session().unwrap();

    // The same place again keeps the binding.
    let data = state.current().data_dir;
    change_place(
        &LoginGate::default(),
        &*store,
        &state,
        &library,
        Mode::Local,
        Some(data),
        None,
        &nobody_listens(),
    )
    .unwrap();
    assert!(!session.is_cancelled());
    assert_eq!(events.states(), [LibraryState::Ready]);

    // Another server forgets the account: the library is logged out.
    change_place(
        &LoginGate::default(),
        &*store,
        &state,
        &library,
        Mode::Remote,
        None,
        Some("http://127.0.0.1:41003".into()),
        &nobody_listens(),
    )
    .unwrap();
    assert!(session.is_cancelled());
    assert_eq!(
        events.states(),
        [LibraryState::Ready, LibraryState::LoggedOut]
    );
    assert_eq!(leaves(&events), [false, true]);
}

fn code_answer() -> String {
    answer(
        "200 OK",
        r#"{"randomCode":"c0de42","timestamp":1763300000000}"#,
    )
}

fn token_answer(token: &str) -> String {
    answer(
        "201 Created",
        &format!(r#"{{"token":"{token}","id":"t1","slug":"jordi","userId":7,"scope":"write"}}"#),
    )
}

#[test]
fn a_login_binds_the_library() {
    let server = Scripted::start(vec![code_answer(), token_answer(TOKEN), ok()]);
    let dir = tempfile::tempdir().unwrap();
    let config = AppConfig {
        mode: Mode::Remote,
        server_url: Some(format!("http://127.0.0.1:{}", server.port)),
        data_dir: dir.path().join("data"),
        ..AppConfig::default()
    };
    config::save(dir.path(), &config).unwrap();
    let state = ConfigState::load(dir.path().to_path_buf());
    let store = Arc::new(MemoryStore::new());
    let (library, events) = library_of(&state, store.clone());
    library.refresh();
    assert_eq!(library.state().unwrap().state, LibraryState::LoggedOut);

    log_in_to(
        &LoginGate::default(),
        &state,
        &ServeInfo::default(),
        &*store,
        &library,
        "me@example.org",
        &Password::new("pw"),
    )
    .unwrap();

    assert_eq!(
        events.states(),
        [LibraryState::LoggedOut, LibraryState::Ready]
    );
    // A login gives the place an account: the review leaves.
    assert_eq!(leaves(&events), [false, true]);
    server.next();
    server.next();
    get_account(&library).unwrap();
    assert_eq!(
        server.next().header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
}

#[test]
fn each_state_has_its_text_and_settings_page() {
    use LibraryState::*;
    let table = [
        (
            NotSetUp,
            "notSetUp",
            Some("Choose where the server runs in Settings."),
            Some("server"),
        ),
        (
            LoggedOut,
            "loggedOut",
            Some("Log in to see your library."),
            Some("account"),
        ),
        (
            Unreachable,
            "unreachable",
            Some("The server does not answer."),
            Some("server"),
        ),
        (
            NotImported,
            "notImported",
            Some("The Supernote has not been imported yet."),
            Some("supernote"),
        ),
        (
            Refused,
            "refused",
            Some("The server refused the login. Log in again."),
            Some("account"),
        ),
        (Ready, "ready", None, None),
    ];
    for (state, name, text, page) in table {
        assert_eq!(state.text(), text, "{name}");
        assert_eq!(state.settings_page(), page, "{name}");
        assert_eq!(
            serde_json::to_value(state_event(state)).unwrap(),
            json!({ "type": "state", "state": name, "text": text, "page": page }),
        );
    }
    // Every page is one of the settings window.
    let pages = include_str!("../../../src/settings/pages.ts");
    for (state, ..) in table {
        if let Some(page) = state.settings_page() {
            assert!(pages.contains(&format!("\"{page}\"")), "{page}");
        }
    }

    // The state each place binds to.
    let dir = tempfile::tempdir().unwrap();
    let bound = |config: AppConfig, store: Arc<MemoryStore>| {
        let place = Place::new(config);
        place.0.lock().unwrap().1.listen = vec!["127.0.0.1:41004".parse().unwrap()];
        let (library, _) = make(store, place.func());
        library.refresh();
        library.state().unwrap().state
    };
    let local = |account: Option<&str>| AppConfig {
        mode: Mode::Local,
        data_dir: dir.path().to_path_buf(),
        account: account.map(str::to_owned),
        ..AppConfig::default()
    };
    let empty = || Arc::new(MemoryStore::new());
    let with_token = || {
        let store = Arc::new(MemoryStore::new());
        store.set("local|jordi", &Token::new(TOKEN)).unwrap();
        store
    };
    assert_eq!(bound(AppConfig::default(), with_token()), NotSetUp);
    let remote_without_address = AppConfig {
        mode: Mode::Remote,
        account: Some("jordi".into()),
        ..AppConfig::default()
    };
    assert_eq!(bound(remote_without_address, with_token()), NotSetUp);
    assert_eq!(bound(local(None), with_token()), LoggedOut);
    assert_eq!(bound(local(Some("jordi")), empty()), LoggedOut);
    assert_eq!(bound(local(Some("jordi")), with_token()), Ready);
}

#[test]
fn a_401_drops_the_token_and_says_so() {
    let server = Scripted::start(vec![answer(
        "401 Unauthorized",
        r#"{"error":"token_refused"}"#,
    )]);
    let dir = tempfile::tempdir().unwrap();
    let place = Place::local(dir.path(), server.port, Some("jordi"));
    let store = CountingStore::with("local|jordi", TOKEN);
    let (library, events) = make(store.clone(), place.func());
    library.refresh();
    let session = library.session().unwrap();

    assert_eq!(
        get_account(&library).unwrap_err(),
        LibraryError::Rest(RestError::TokenRefused)
    );
    server.next();
    assert_eq!(
        library.state().unwrap(),
        StateView {
            state: LibraryState::Refused,
            text: Some("The server refused the login. Log in again.".into()),
            page: Some("account".into()),
        }
    );
    assert_eq!(
        events.states(),
        [LibraryState::Ready, LibraryState::Refused]
    );
    assert!(session.is_cancelled(), "the jobs of the refused login end");
    // The token is gone from the library: no call is made with it again.
    assert_eq!(
        get_account(&library).unwrap_err(),
        LibraryError::NotReady(LibraryState::Refused)
    );
    assert!(server.received().is_empty());

    // A new address of the same server does not bring the refused token back.
    place.listen_on(41005);
    library.refresh();
    assert_eq!(library.state().unwrap().state, LibraryState::Refused);
    assert_eq!(store.gets(), 1);

    // A login reads the token again.
    store.set("local|jordi", &Token::new("tok-new")).unwrap();
    library.relogin();
    assert_eq!(library.state().unwrap().state, LibraryState::Ready);

    // A 401 of a call made under the old login does not drop the new one.
    library.settle(&session, &Err::<(), _>(RestError::TokenRefused));
    assert_eq!(library.state().unwrap().state, LibraryState::Ready);
    assert!(library.session().is_ok());
}

#[test]
fn the_state_follows_the_answers() {
    let dir = tempfile::tempdir().unwrap();
    let place = Place::local(dir.path(), 41006, Some("jordi"));
    let store = CountingStore::with("local|jordi", TOKEN);
    let (library, events) = make(store, place.func());
    library.refresh();
    let session = library.session().unwrap();

    library.settle(&session, &Err::<(), _>(RestError::Unreachable));
    assert_eq!(library.state().unwrap().state, LibraryState::Unreachable);
    // Still bound: the next call may find the server.
    assert!(!session.is_cancelled());
    library.settle(&session, &Ok(()));
    assert_eq!(library.state().unwrap().state, LibraryState::Ready);
    library.settle(&session, &Err::<(), _>(RestError::NotImported));
    assert_eq!(library.state().unwrap().state, LibraryState::NotImported);
    // Other errors belong to the call, not to the library.
    library.settle(&session, &Err::<(), _>(RestError::NotFound));
    assert_eq!(library.state().unwrap().state, LibraryState::NotImported);
    library.settle(&session, &Ok(()));
    library.settle(&session, &Ok(()));
    // Only a binding lists again; a settled call only says the state.
    assert_eq!(
        events.all(),
        [
            state_event(LibraryState::Ready),
            relist(false),
            state_event(LibraryState::Unreachable),
            state_event(LibraryState::Ready),
            state_event(LibraryState::NotImported),
            state_event(LibraryState::Ready)
        ]
    );
}

#[test]
fn no_library_event_holds_the_token() {
    // The events of a real run: bind, a server that does not answer, a 401, a new login.
    let server = Scripted::start(vec![answer(
        "401 Unauthorized",
        r#"{"error":"token_refused"}"#,
    )]);
    let dir = tempfile::tempdir().unwrap();
    let place = Place::local(dir.path(), server.port, Some("jordi"));
    let store = CountingStore::with("local|jordi", TOKEN);
    let (library, events) = make(store.clone(), place.func());
    library.refresh();
    let session = library.session().unwrap();
    library.settle(&session, &Err::<(), _>(RestError::Unreachable));
    let refused = get_account(&library).unwrap_err();
    library.relogin();
    place.change(|c| c.account = None);
    library.refresh();
    let mut all = events.all();
    assert!(all.len() >= 4, "{all:?}");

    // And every kind of event, filled.
    let entry = Entry {
        id: 7,
        path: "/Note/a.note".into(),
        name: "a.note".into(),
        kind: EntryKind::File,
        size: Some(12),
        md5: Some("0123456789abcdef0123456789abcdef".into()),
        mtime_ms: 1_763_300_000_000,
        writable: true,
    };
    let folder = LibraryEvent::Folder {
        folder: FolderState {
            path: "/Note".into(),
            entries: vec![entry],
            complete: false,
            loading: true,
            error: Some("The server does not answer.".into()),
        },
        folders_only: true,
        next: Some("t1:a.note".into()),
        revision: 6,
    };
    let job = LibraryEvent::Job {
        job: Job {
            id: 3,
            kind: JobKind::Upload,
            label: "a.note".into(),
            done: 5,
            total: Some(12),
            state: JobState::Asking,
            text: None,
            question: Some(Clash {
                name: "a.note".into(),
                existing: "A.note".into(),
                can_replace: true,
            }),
            result: None,
            drop_id: Some(4),
        },
    };
    assert_eq!(
        serde_json::to_value(&folder).unwrap(),
        json!({ "type": "folder", "folder": {
            "path": "/Note",
            "entries": [{ "id": 7, "path": "/Note/a.note", "name": "a.note", "kind": "file",
                "size": 12, "md5": "0123456789abcdef0123456789abcdef",
                "mtimeMs": 1_763_300_000_000_i64, "writable": true }],
            "complete": false, "loading": true, "error": "The server does not answer." },
            "foldersOnly": true, "next": "t1:a.note", "revision": 6 })
    );
    assert_eq!(
        serde_json::to_value(&job).unwrap(),
        json!({ "type": "job", "job": { "id": 3, "kind": "upload", "label": "a.note", "done": 5,
            "total": 12, "state": "asking", "text": null,
            "question": { "name": "a.note", "existing": "A.note", "canReplace": true },
            "result": null, "dropId": 4 } })
    );
    assert_eq!(
        serde_json::to_value(relist(true)).unwrap(),
        json!({ "type": "relist", "leave": true })
    );
    assert_eq!(
        serde_json::to_value(LibraryEvent::Recycle).unwrap(),
        json!({ "type": "recycle" })
    );
    let dropped = LibraryEvent::Drop {
        drop_id: 4,
        position: Position { x: 120, y: 48 },
        count: 2,
    };
    assert_eq!(
        serde_json::to_value(&dropped).unwrap(),
        json!({ "type": "drop", "dropId": 4, "position": { "x": 120, "y": 48 }, "count": 2 })
    );
    all.extend([folder, job, relist(true), LibraryEvent::Recycle, dropped]);

    let token = TOKEN;
    for event in &all {
        let text = serde_json::to_string(event).unwrap();
        assert!(!text.contains(token), "{text}");
        assert!(!format!("{event:?}").contains(token));
        assert!(!format!("{event:#?}").contains(token));
    }
    for shown in [
        serde_json::to_string(&library.state().unwrap()).unwrap(),
        refused.to_string(),
        format!("{refused:?}"),
    ] {
        assert!(!shown.contains(token), "{shown}");
    }
}

/// A store whose reads fail until `fixed` is set.
#[derive(Default)]
struct BrokenStore {
    inner: MemoryStore,
    fixed: AtomicBool,
    gets: AtomicUsize,
}

impl TokenStore for BrokenStore {
    fn get(&self, key: &str) -> Result<Option<Token>, TokenError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        if self.fixed.load(Ordering::SeqCst) {
            self.inner.get(key)
        } else {
            Err(TokenError("locked".into()))
        }
    }

    fn set(&self, key: &str, token: &Token) -> Result<(), TokenError> {
        self.inner.set(key, token)
    }

    fn delete(&self, key: &str) -> Result<(), TokenError> {
        self.inner.delete(key)
    }
}

#[test]
fn a_keychain_that_cannot_be_read_says_so_and_is_tried_again() {
    let dir = tempfile::tempdir().unwrap();
    let place = Place::local(dir.path(), 41007, Some("jordi"));
    let store = Arc::new(BrokenStore::default());
    store.inner.set("local|jordi", &Token::new(TOKEN)).unwrap();
    let (library, events) = make(store.clone(), place.func());

    library.refresh();
    let view = library.state().unwrap();
    assert_eq!(
        view,
        StateView {
            state: LibraryState::LoggedOut,
            text: Some("The Keychain could not be read.".into()),
            page: Some("account".into()),
        }
    );
    assert!(library.session().is_err());

    // The next nudge for the same place reads again.
    store.fixed.store(true, Ordering::SeqCst);
    library.refresh();
    assert_eq!(store.gets.load(Ordering::SeqCst), 2);
    assert_eq!(library.state().unwrap().state, LibraryState::Ready);
    assert_eq!(
        events.states(),
        [LibraryState::LoggedOut, LibraryState::Ready]
    );
    // A failed binding records no place: binding again after it leaves no review.
    assert_eq!(leaves(&events), [false, false]);
    for event in events.all() {
        assert!(!serde_json::to_string(&event).unwrap().contains(TOKEN));
    }
}

#[test]
fn before_the_first_binding_the_state_is_not_known() {
    let place = Place::new(AppConfig::default());
    let (library, events) = make(Arc::new(MemoryStore::new()), place.func());
    assert_eq!(library.state(), None);
    assert!(events.all().is_empty());
    library.refresh();
    assert_eq!(library.state(), Some(StateView::of(LibraryState::NotSetUp)));
}
