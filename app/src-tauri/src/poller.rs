//! The poller: it calls `notedown pipeline status` and `GET /account` on their own cadence,
//! keeps the last answers and turns them into a [`Status`] whenever something changed.
//!
//! Cadence: `pipeline status` every 5 s while a window or the panel is visible and the queue
//! is not empty, 15 s while visible otherwise, 60 s when hidden; `/account` every 10 s while
//! visible, 60 s when hidden. A poll never starts while the last one of its kind still runs.
//! [`Poller::tick`] starts what is due, each call on a thread of its own; the app ticks once a
//! second.

use reqwest::header::{AUTHORIZATION, HeaderValue};
use std::process::Output;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::config::{self, AppConfig, ExistingSetup, Mode, SyncMark};
use crate::process::{self, SpawnFn};
use crate::processes::ProcessStates;
use crate::serve_info::{self, ServeInfo};
use crate::status::{
    AccountAnswer, AccountInfo, AccountPoll, Parts, PipelineCall, PipelineFacts, Status,
    status_from,
};
use crate::supervisor::Clock;
use crate::token::{Token, TokenStore, token_key};

/// How often the app calls [`Poller::tick`].
pub const TICK_EVERY: Duration = Duration::from_secs(1);

/// The longest a `pipeline status` call may take before it is ended.
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(60);

/// The connect and the whole-request timeouts of `GET /account`.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

/// The time between two `pipeline status` calls.
pub fn pipeline_every(visible: bool, queue_not_empty: bool) -> Duration {
    Duration::from_secs(match (visible, queue_not_empty) {
        (true, true) => 5,
        (true, false) => 15,
        (false, _) => 60,
    })
}

/// The time between two `/account` polls.
pub fn account_every(visible: bool) -> Duration {
    Duration::from_secs(if visible { 10 } else { 60 })
}

/// The arguments of `pipeline status`: `--account` only when the config names one.
pub fn pipeline_status_args(config: &AppConfig) -> Vec<String> {
    let mut args = vec![
        "pipeline".to_owned(),
        "status".to_owned(),
        "--data".to_owned(),
        config.data_dir.to_string_lossy().into_owned(),
    ];
    if let Some(account) = config.account.as_deref().filter(|a| !a.is_empty()) {
        args.extend(["--account".to_owned(), account.to_owned()]);
    }
    args
}

/// The REST base the app polls: its own server on loopback in Local mode, the configured
/// address in Remote mode, none without a mode or an address.
pub fn rest_base(config: &AppConfig, serve_info: &ServeInfo) -> Option<String> {
    match config.mode {
        Mode::NotSet => None,
        Mode::Local => Some(serve_info::local_rest_base(
            serve_info,
            serve_info::server_toml_addr(&config.data_dir),
        )),
        Mode::Remote => {
            let url = config.server_url.as_deref()?.trim().trim_end_matches('/');
            if url.is_empty() {
                None
            } else if url.ends_with("/rest/v1") {
                Some(url.to_owned())
            } else {
                Some(format!("{url}/rest/v1"))
            }
        }
    }
}

/// Where the answers come from: the sidecar and the server, or fakes in the tests.
pub trait Sources: Send + Sync {
    /// Runs `notedown` with `args` (`pipeline status ...`).
    fn pipeline_status(&self, args: &[String]) -> PipelineCall;
    /// `GET <base>/account`, with the token when there is one.
    fn account(&self, base: &str, token: Option<&Token>) -> AccountAnswer;
    /// The highest journal number of the token's account: the `end` of `GET <base>/events`.
    fn events_end(&self, base: &str, token: &Token) -> Result<u64, String>;
}

/// What the poller reads from the rest of the app at each tick.
#[derive(Debug, Clone)]
pub struct Inputs {
    pub config: AppConfig,
    pub config_reset: bool,
    pub processes: ProcessStates,
    pub serve_info: ServeInfo,
}

pub type InputsFn = Arc<dyn Fn() -> Inputs + Send + Sync>;

type Listener = Box<dyn Fn(&Status) + Send + Sync>;
type MarkListener = Box<dyn Fn(SyncMark) + Send + Sync>;

/// The status poller. Cheap to share: clones use the same state.
#[derive(Clone)]
pub struct Poller {
    inner: Arc<Inner>,
}

struct Inner {
    sources: Arc<dyn Sources>,
    tokens: Arc<dyn TokenStore>,
    inputs: InputsFn,
    clock: Clock,
    state: Mutex<State>,
    /// Taken before `state` while a status is made and handed out, so listeners see the
    /// statuses in order.
    listeners: Mutex<Vec<Listener>>,
    /// Gets the sync mark of an account on another machine (see [`Poller::on_sync_mark`]).
    marks: Mutex<Option<MarkListener>>,
    stopped: AtomicBool,
}

#[derive(Default)]
struct State {
    visible: bool,
    pipeline: Slot<PipelineCall>,
    account: Slot<AccountPoll>,
    status: Option<Status>,
    existing: Option<Existing>,
    /// When the last call for the mark of a server on another machine failed.
    mark_failed_ms: Option<u64>,
}

/// The least time between two failed calls for the mark of a server on another machine.
const MARK_RETRY_MS: u64 = 60_000;

/// What the data folder held when it was last looked at.
struct Existing {
    dir: std::path::PathBuf,
    setup: ExistingSetup,
    at_ms: u64,
}

/// Without a mode nothing is polled; the setup still sees a new folder within this time.
const EXISTING_EVERY_MS: u64 = 15_000;

/// One kind of poll: its last result, whether a call runs and when the last one ended.
struct Slot<T> {
    /// The config the result belongs to; a result of another config is dropped.
    key: Option<Key>,
    last: Option<T>,
    busy: bool,
    ended_ms: Option<u64>,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Self {
            key: None,
            last: None,
            busy: false,
            ended_ms: None,
        }
    }
}

/// What a result depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Key {
    mode: Mode,
    data_dir: std::path::PathBuf,
    account: Option<String>,
    server_url: Option<String>,
}

impl Key {
    fn of(config: &AppConfig) -> Self {
        Self {
            mode: config.mode,
            data_dir: config.data_dir.clone(),
            account: config.account.clone(),
            server_url: config.server_url.clone(),
        }
    }
}

impl<T> Slot<T> {
    /// A result of another config is no result: start over.
    fn follow(&mut self, key: &Key) {
        if self.key.as_ref() != Some(key) {
            self.key = Some(key.clone());
            self.last = None;
            self.ended_ms = None;
        }
    }

    /// The last result, when it belongs to `key`.
    fn of(&self, key: &Key) -> Option<&T> {
        self.last
            .as_ref()
            .filter(|_| self.key.as_ref() == Some(key))
    }

    /// Not running, and `every` has passed since the last call ended.
    fn due(&self, now_ms: u64, every: Duration) -> bool {
        let every = u64::try_from(every.as_millis()).unwrap_or(u64::MAX);
        !self.busy
            && self
                .ended_ms
                .is_none_or(|end| now_ms >= end.saturating_add(every))
    }
}

/// The pages the pipeline still has to read; the 5 s cadence is for them.
fn queue_not_empty(call: Option<&PipelineCall>) -> bool {
    matches!(call, Some(PipelineCall::Facts(f)) if f.queued + f.pending > 0)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Poller {
    pub fn new(
        sources: Arc<dyn Sources>,
        tokens: Arc<dyn TokenStore>,
        inputs: InputsFn,
        clock: Clock,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                sources,
                tokens,
                inputs,
                clock,
                state: Mutex::new(State::default()),
                listeners: Mutex::new(Vec::new()),
                marks: Mutex::new(None),
                stopped: AtomicBool::new(false),
            }),
        }
    }

    /// Starts every poll that is due, then makes the status again. Never waits for a call.
    pub fn tick(&self) {
        Inner::tick(&self.inner);
    }

    /// A window or the panel became visible or hidden. A poll that is due by the visible
    /// cadence starts at once.
    pub fn set_visible(&self, visible: bool) {
        lock(&self.inner.state).visible = visible;
        self.tick();
    }

    /// Polls at once what is not running (the panel opened).
    pub fn poll_now(&self) {
        {
            let mut state = lock(&self.inner.state);
            state.pipeline.ended_ms = None;
            state.account.ended_ms = None;
        }
        self.tick();
    }

    /// Makes the status again from the last answers and tells the listeners when it changed
    /// (a child changed its state).
    pub fn refresh(&self) {
        self.inner.refresh();
    }

    /// The status as it is now, from the last answers. Cheap: it waits for no call.
    pub fn status(&self) -> Status {
        self.inner.make(&(self.inner.inputs)())
    }

    /// `cb` gets every new status. It runs on a poller thread and must not block or call
    /// [`Poller::on_status`].
    pub fn on_status(&self, cb: impl Fn(&Status) + Send + Sync + 'static) {
        lock(&self.inner.listeners).push(Box::new(cb));
    }

    /// `cb` gets the sync mark of an account on another machine: the journal end when a poll
    /// first sees its files imported while the config has no mark for it. It runs on a poller
    /// thread; the config checks again that the mark still belongs to the account.
    pub fn on_sync_mark(&self, cb: impl Fn(SyncMark) + Send + Sync + 'static) {
        *lock(&self.inner.marks) = Some(Box::new(cb));
    }

    /// No poll runs.
    #[cfg(test)]
    pub(crate) fn idle(&self) -> bool {
        let state = lock(&self.inner.state);
        !state.pipeline.busy && !state.account.busy
    }

    /// Starts no poll any more (Quit).
    pub fn stop(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
    }
}

impl Inner {
    fn tick(this: &Arc<Self>) {
        let inputs = (this.inputs)();
        let key = Key::of(&inputs.config);
        let now = (this.clock)();
        let polling = inputs.config.mode != Mode::NotSet && !this.stopped.load(Ordering::SeqCst);
        let base = rest_base(&inputs.config, &inputs.serve_info);
        let (pipeline, account) = {
            let mut state = lock(&this.state);
            state.pipeline.follow(&key);
            state.account.follow(&key);
            let visible = state.visible;
            let queue = queue_not_empty(state.pipeline.of(&key));
            let pipeline = polling && state.pipeline.due(now, pipeline_every(visible, queue));
            let account =
                polling && base.is_some() && state.account.due(now, account_every(visible));
            state.pipeline.busy |= pipeline;
            state.account.busy |= account;
            // The data folder is looked at once per `pipeline status` call, and now and then
            // while there is no mode; never on each tick.
            let dir = &inputs.config.data_dir;
            let stale = match &state.existing {
                None => true,
                Some(e) if &e.dir != dir => true,
                Some(e) => {
                    pipeline
                        || (inputs.config.mode == Mode::NotSet
                            && now >= e.at_ms.saturating_add(EXISTING_EVERY_MS))
                }
            };
            if stale {
                state.existing = Some(Existing {
                    dir: dir.clone(),
                    setup: config::existing_setup(dir),
                    at_ms: now,
                });
            }
            (pipeline, account)
        };
        if pipeline {
            let this = this.clone();
            let config = inputs.config.clone();
            std::thread::spawn(move || this.poll_pipeline(&config));
        }
        if let (true, Some(base)) = (account, base) {
            let this = this.clone();
            let config = inputs.config.clone();
            std::thread::spawn(move || this.poll_account(&config, &base));
        }
        this.refresh();
    }

    fn poll_pipeline(&self, config: &AppConfig) {
        let call = self.sources.pipeline_status(&pipeline_status_args(config));
        let key = Key::of(config);
        let current = self.current_key();
        {
            let mut state = lock(&self.state);
            let slot = &mut state.pipeline;
            if current == key && slot.key.as_ref() == Some(&key) {
                slot.last = Some(call);
                slot.ended_ms = Some((self.clock)());
            }
        }
        self.refresh();
        // Busy until the status with this answer is out, so a poll never overlaps its own
        // hand-out.
        lock(&self.state).pipeline.busy = false;
    }

    fn poll_account(&self, config: &AppConfig, base: &str) {
        // The token is read here, on the poll's thread: the Keychain may take its time.
        let (token, answer) = match token_key(config).map(|k| self.tokens.get(&k)) {
            None | Some(Ok(None)) => (None, self.sources.account(base, None)),
            Some(Ok(Some(token))) => {
                let answer = self.sources.account(base, Some(&token));
                (Some(token), answer)
            }
            Some(Err(e)) => (None, AccountAnswer::Failed(e.to_string())),
        };
        let had_token = token.is_some();
        // Taken after the answer is out, so a slow or failing mark never holds the status up.
        let marking = token.filter(|_| self.wants_mark(config, &answer));
        let key = Key::of(config);
        let current = self.current_key();
        {
            let now = (self.clock)();
            let mut state = lock(&self.state);
            let slot = &mut state.account;
            if current == key && slot.key.as_ref() == Some(&key) {
                // Not answering counts from the first poll of a row without an answer.
                let unreachable_since_ms = match answer {
                    AccountAnswer::Unreachable(_) => Some(
                        slot.last
                            .as_ref()
                            .and_then(|p| p.unreachable_since_ms)
                            .unwrap_or(now),
                    ),
                    _ => None,
                };
                slot.last = Some(AccountPoll {
                    had_token,
                    answer,
                    unreachable_since_ms,
                });
                slot.ended_ms = Some(now);
            }
        }
        self.refresh();
        if let Some(token) = marking {
            self.mark_remote(config, base, &token);
        }
        lock(&self.state).account.busy = false;
    }

    /// On another machine the import is done there: the first poll that sees the files
    /// imported, with no mark for the account yet, takes the mark (see [`mark_remote`]).
    ///
    /// [`mark_remote`]: Self::mark_remote
    fn wants_mark(&self, config: &AppConfig, answer: &AccountAnswer) -> bool {
        let AccountAnswer::Account(info) = answer else {
            return false;
        };
        if config.mode != Mode::Remote || info.imported != Some(true) {
            return false;
        }
        let Some(key) = token_key(config) else {
            return false;
        };
        let now = (self.clock)();
        let tried = lock(&self.state).mark_failed_ms;
        !config
            .setup
            .sync_mark
            .as_ref()
            .is_some_and(|m| m.key == key)
            && lock(&self.marks).is_some()
            && tried.is_none_or(|at| now >= at.saturating_add(MARK_RETRY_MS))
    }

    /// Asks the server for the journal end and hands the mark out. A failed call is tried
    /// again at most once in [`MARK_RETRY_MS`].
    fn mark_remote(&self, config: &AppConfig, base: &str, token: &Token) {
        let Some(key) = token_key(config) else {
            return;
        };
        match self.sources.events_end(base, token) {
            Ok(seq) => {
                lock(&self.state).mark_failed_ms = None;
                if let Some(cb) = lock(&self.marks).as_ref() {
                    cb(SyncMark { key, seq });
                }
            }
            Err(_) => lock(&self.state).mark_failed_ms = Some((self.clock)()),
        }
    }

    fn current_key(&self) -> Key {
        Key::of(&(self.inputs)().config)
    }

    /// The status from `inputs` and the last answers that belong to its config.
    fn make(&self, inputs: &Inputs) -> Status {
        let key = Key::of(&inputs.config);
        let parts = {
            let state = lock(&self.state);
            Parts {
                config: inputs.config.clone(),
                config_reset: inputs.config_reset,
                existing: match &state.existing {
                    Some(e) if e.dir == inputs.config.data_dir => e.setup,
                    // Only before the first tick after a new folder.
                    _ => config::existing_setup(&inputs.config.data_dir),
                },
                server: inputs.processes.server.clone(),
                pipeline: inputs.processes.pipeline.clone(),
                device_urls: inputs.serve_info.device_urls.clone(),
                pipeline_status: state.pipeline.of(&key).cloned(),
                account: state.account.of(&key).cloned(),
            }
        };
        status_from(&parts, (self.clock)())
    }

    /// Makes the status and hands it to the listeners when it changed. The listener lock is
    /// held throughout, so two threads cannot hand out their statuses out of order.
    fn refresh(&self) {
        let listeners = lock(&self.listeners);
        let status = self.make(&(self.inputs)());
        {
            let mut state = lock(&self.state);
            if state.status.as_ref() == Some(&status) {
                return;
            }
            state.status = Some(status.clone());
        }
        for listener in listeners.iter() {
            listener(&status);
        }
    }
}

/// The real sources: the sidecar for `pipeline status` and HTTP for `/account`.
pub struct RealSources {
    spawn: SpawnFn,
    /// Made on the first poll, on a poller thread: a blocking client must not be made inside
    /// an async runtime. One for the app's own server on loopback, one for another machine.
    clients: Mutex<Clients>,
}

#[derive(Default)]
struct Clients {
    loopback: Option<reqwest::blocking::Client>,
    remote: Option<reqwest::blocking::Client>,
}

impl RealSources {
    pub fn new(spawn: SpawnFn) -> Self {
        Self {
            spawn,
            clients: Mutex::default(),
        }
    }
}

impl Sources for RealSources {
    fn pipeline_status(&self, args: &[String]) -> PipelineCall {
        run_status(&self.spawn, args, STATUS_TIMEOUT)
    }

    fn events_end(&self, base: &str, token: &Token) -> Result<u64, String> {
        crate::import_job::events_end(base, token.clone())
    }

    fn account(&self, base: &str, token: Option<&Token>) -> AccountAnswer {
        let client = match self.client(is_loopback(base)) {
            Ok(client) => client,
            Err(why) => return AccountAnswer::Failed(why),
        };
        let mut request = client.get(format!("{base}/account"));
        if let Some(token) = token {
            // The value never shows in an error or a log line: only the header carries it.
            let Ok(mut value) = HeaderValue::from_str(&format!("Bearer {}", token.expose())) else {
                return AccountAnswer::Failed("the stored login cannot be sent".to_owned());
            };
            value.set_sensitive(true);
            request = request.header(AUTHORIZATION, value);
        }
        let response = match request.send() {
            Ok(response) => response,
            Err(e) if e.is_timeout() => {
                return AccountAnswer::Unreachable("no answer in time".to_owned());
            }
            Err(e) if e.is_connect() => {
                return AccountAnswer::Unreachable(format!("no connection to {}", shown(base)));
            }
            // Without the address: it could carry a user and a password.
            Err(e) => {
                return AccountAnswer::Failed(format!("the request failed: {}", e.without_url()));
            }
        };
        match response.status().as_u16() {
            200 => match response.bytes() {
                Ok(body) => match serde_json::from_slice::<AccountInfo>(&body) {
                    Ok(info) => AccountAnswer::Account(info),
                    Err(e) => AccountAnswer::Failed(format!("the answer could not be read: {e}")),
                },
                Err(e) if e.is_timeout() => {
                    AccountAnswer::Unreachable("no answer in time".to_owned())
                }
                // Without the address, as above.
                Err(e) => AccountAnswer::Failed(format!(
                    "the answer could not be read: {}",
                    e.without_url()
                )),
            },
            401 => AccountAnswer::Refused,
            410 => AccountAnswer::Failed("the account was removed".to_owned()),
            // A proxy in front of a server that is down. A Notedown server that answers 503
            // itself (store_damaged) sends its JSON error and is there.
            code @ (502..=504) => {
                let own_error = code == 503
                    && response
                        .bytes()
                        .ok()
                        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                        .is_some_and(|v| v.get("error").is_some());
                if own_error {
                    AccountAnswer::Failed(format!("the server answered {code}"))
                } else {
                    AccountAnswer::Unreachable(format!(
                        "the server behind {} does not answer ({code})",
                        shown(base)
                    ))
                }
            }
            code => AccountAnswer::Failed(format!("the server answered {code}")),
        }
    }
}

/// Whether `base` is on this Mac: a loopback address or `localhost`.
pub(crate) fn is_loopback(base: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let ip = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || ip
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// The address for a line of text, without a user or a password in it.
pub(crate) fn shown(base: &str) -> String {
    match reqwest::Url::parse(base) {
        Ok(mut url) => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.as_str().trim_end_matches('/').to_owned()
        }
        Err(_) => "the server".to_owned(),
    }
}

impl RealSources {
    /// The client for `loopback` or another machine, made once. Neither follows a redirect: a
    /// Notedown server never sends one, so a redirect is shown as "the server answered 301"
    /// and the token never goes to another address. The loopback client uses no proxy: the
    /// app's own server is on this Mac, and a system proxy must not see its token.
    fn client(&self, loopback: bool) -> Result<reqwest::blocking::Client, String> {
        let mut clients = lock(&self.clients);
        let slot = if loopback {
            &mut clients.loopback
        } else {
            &mut clients.remote
        };
        if let Some(client) = slot.as_ref() {
            return Ok(client.clone());
        }
        let mut builder = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none());
        if loopback {
            builder = builder.no_proxy();
        }
        let made = builder
            .build()
            .map_err(|e| format!("no HTTP client: {e}"))?;
        *slot = Some(made.clone());
        Ok(made)
    }
}

/// Runs `pipeline status` and reads its answer; ends it after `timeout`.
pub fn run_status(spawn: &SpawnFn, args: &[String], timeout: Duration) -> PipelineCall {
    match process::run_bounded(spawn, args, timeout) {
        Ok(output) => read_status(&output),
        Err(why) => PipelineCall::Failed(why),
    }
}

fn read_status(output: &Output) -> PipelineCall {
    if output.status.success() {
        return match serde_json::from_slice::<PipelineFacts>(&output.stdout) {
            Ok(facts) => PipelineCall::Facts(facts),
            Err(e) => PipelineCall::Failed(format!("the answer could not be read: {e}")),
        };
    }
    PipelineCall::Failed(process::error_text(&output.stderr, output.status))
}

#[cfg(test)]
mod tests;
