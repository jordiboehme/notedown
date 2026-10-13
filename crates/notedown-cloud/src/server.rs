//! The cloud server: state, router and listeners.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::net::TcpListener;

use crate::api::limit::{RateLimiter, RetryTracker};
use crate::auth::Auth;
use crate::diag::{Capture, Clock, LogEntry, LogKind, ProtocolLog, SystemClock};
use crate::journal::{Journal, JournalOptions};
use crate::push::{PushPlanner, PushTiming};
use crate::store::{ScanHold, ScanOptions, ScanReport, Store, Walk, WriteGuard};
use crate::sync::staging::{self, StagingLocks};
use crate::sync::{SyncLocks, UrlSigner};
use crate::watch::Scope;
use crate::{Config, Error, Result};

pub use crate::api::router;

/// Request bodies up to this size are read (and logged, cut at the log's own cap). A larger body
/// is not read further and the request is answered with an envelope.
pub const MAX_REQUEST_BODY: usize = 8 * 1024 * 1024;

/// Everything the routes share.
pub struct AppState {
    pub config: Config,
    pub data_dir: PathBuf,
    pub auth: Auth,
    pub log: ProtocolLog,
    pub clock: Arc<dyn Clock>,
    /// The change journal (server-wide).
    pub journal: Journal,
    pub(crate) limiter: RateLimiter,
    pub(crate) retries: RetryTracker,
    /// The sync session of each account (`synchronous/start|end`).
    pub sync_locks: SyncLocks,
    /// The live auto-sync socket sessions (pushes are handed to them).
    pub sockets: crate::socket::SocketRegistry,
    /// Which device gets a push and when (fed by the journal follower, `crate::push::Follower`).
    pub push: Mutex<PushPlanner>,
    /// Signs and checks the transfer URLs of `/api/oss/*` (`keys/url.key`).
    pub url_signer: UrlSigner,
    /// One open [`Store`] per account, shared by the requests.
    stores: Mutex<HashMap<String, Arc<Store>>>,
    /// `usedCapacity` per account: (computed at ms, bytes).
    usage: Mutex<HashMap<String, (u64, u64)>>,
    /// Serializes the part merge of each staging entry.
    pub(crate) staging_locks: StagingLocks,
    /// The largest part of a chunked upload ([`staging::PART_MAX`]).
    part_max: AtomicU64,
    /// The largest single upload or merged file ([`staging::FILE_MAX`]).
    file_max: AtomicU64,
}

/// What [`AppState::scan_account`] came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanOutcome {
    /// The scan ran (its errors were logged).
    Done,
    /// A device holds the sync lock of the account: try again later.
    Busy,
    /// Nothing to scan: no account, no `files/` folder, or the protect gate is closed.
    Skipped,
}

/// Marker flag: the file system of the data folder changes the Unicode normalization of file
/// names (the probe of `Store::open`), so lookups by the device's names can fail.
pub const FLAG_NAMES_NOT_PRESERVED: &str = "names_not_preserved";

/// How long a computed `usedCapacity` is reused (ms).
pub const USAGE_CACHE_MS: u64 = 60_000;

impl AppState {
    pub fn open(config: Config, data_dir: &Path) -> Result<Arc<AppState>> {
        Self::open_with_clock(config, data_dir, Arc::new(SystemClock))
    }

    /// With an injected clock (tests): it drives login times, limits, the log and the journal.
    /// Unfinished commits of every account are recovered here (`Store::recover`).
    pub fn open_with_clock(
        config: Config,
        data_dir: &Path,
        clock: Arc<dyn Clock>,
    ) -> Result<Arc<AppState>> {
        let log = ProtocolLog::open_with_clock(
            data_dir,
            config.log_retention_days,
            config.log_max_bytes,
            clock.clone(),
        )?;
        let log = if config.capture {
            log.with_capture(Capture::open(data_dir)?)
        } else {
            log
        };
        let journal = Journal::open_with(
            data_dir,
            JournalOptions {
                clock: clock.clone(),
                ..JournalOptions::default()
            },
        )?;
        // Tombstones of removed accounts that a crash left (errors are logged).
        match crate::auth::Accounts::new(data_dir).sweep_tombstones() {
            0 => {}
            n => eprintln!("notedown cloud: deleted {n} folders of removed accounts"),
        }
        // The stores recovery opened are kept: the start loads each registry once.
        let (recovered, opened) = crate::store::recover_all_keeping(data_dir, &journal)?;
        for (slug, r) in recovered {
            match r {
                Ok(r) => eprintln!("notedown cloud: account {slug}: {r}"),
                Err(e) => {
                    let message = format!("account {slug}: recovery failed: {e}");
                    eprintln!("notedown cloud: {message}");
                    let mut m = LogEntry::new(LogKind::Marker);
                    m.flags = vec![crate::diag::FLAG_RECOVERY_FAILED.to_string()];
                    m.message = Some(message);
                    log.record(m);
                }
            }
        }
        match staging::sweep(data_dir, now_ms_of(clock.as_ref())) {
            Ok(0) => {}
            Ok(n) => eprintln!("notedown cloud: removed {n} stale upload staging entries"),
            Err(e) => eprintln!("notedown cloud: staging sweep failed: {e}"),
        }
        report_temp_sweep(crate::store::sweep_all_temps_with(data_dir, &opened));
        let stores = opened
            .into_iter()
            .map(|store| {
                log_names_not_preserved(&log, &store);
                (store.slug().to_string(), Arc::new(store))
            })
            .collect();
        Ok(Arc::new(AppState {
            auth: Auth::open(data_dir)?,
            url_signer: UrlSigner::load_or_create(data_dir)?,
            sync_locks: SyncLocks::new(),
            sockets: crate::socket::SocketRegistry::new(),
            push: Mutex::new(PushPlanner::new(PushTiming::default())),
            stores: Mutex::new(stores),
            usage: Mutex::new(HashMap::new()),
            staging_locks: StagingLocks::default(),
            part_max: AtomicU64::new(staging::PART_MAX),
            file_max: AtomicU64::new(staging::FILE_MAX),
            journal,
            log,
            clock,
            data_dir: data_dir.to_path_buf(),
            config,
            limiter: RateLimiter::default(),
            retries: RetryTracker::default(),
        }))
    }

    /// Whether this run exchanges files or refuses every sync.
    pub fn sync_mode(&self) -> crate::SyncMode {
        self.config.sync
    }

    /// The store of account `slug`, opened on first use and then shared. Blocking (it may take
    /// the store lock): call it from a blocking task.
    pub fn store(&self, slug: &str) -> Result<Arc<Store>> {
        let stores = || self.stores.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = stores().get(slug) {
            return Ok(s.clone());
        }
        // Opened without the map lock: `Store::open` waits for the account's store lock (a CLI
        // import may hold it), and other accounts must not wait behind that.
        let store = Arc::new(Store::open(&self.data_dir, slug)?);
        log_names_not_preserved(&self.log, &store);
        Ok(stores().entry(slug.to_string()).or_insert(store).clone())
    }

    /// The bytes of every file the account's listing shows, reused for [`USAGE_CACHE_MS`].
    /// Blocking: it walks the whole tree under the store lock (no ids, no hashing).
    pub fn used_bytes(&self, slug: &str) -> Result<u64> {
        let now = self.now_ms();
        if let Some((at, used)) = self
            .usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(slug)
            .copied()
            && now.saturating_sub(at) < USAGE_CACHE_MS
        {
            return Ok(used);
        }
        let store = self.store(slug)?;
        let guard = store.lock_recovered(&self.journal)?;
        // No listing: it would give hand-added files ids without a scan journaling them.
        let used = store.disk_bytes(&guard)?;
        drop(guard);
        self.usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(slug.to_string(), (now, used));
        Ok(used)
    }

    /// Forget the cached `usedCapacity` of `slug` (after an upload or another change).
    pub fn invalidate_usage(&self, slug: &str) {
        self.usage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(slug);
    }

    /// The largest part of a chunked upload in bytes.
    pub fn part_max(&self) -> u64 {
        self.part_max.load(Ordering::Relaxed)
    }

    /// The largest single upload or merged file in bytes.
    pub fn file_max(&self) -> u64 {
        self.file_max.load(Ordering::Relaxed)
    }

    /// Lower the upload limits (tests). Not for production use.
    #[doc(hidden)]
    pub fn set_upload_limits(&self, part_max: u64, file_max: u64) {
        self.part_max.store(part_max, Ordering::Relaxed);
        self.file_max.store(file_max, Ordering::Relaxed);
    }

    /// Remove staging entries older than 24 h (by the clock). Blocking. The server runs it on
    /// start and every hour.
    pub fn sweep_staging(&self) -> Result<usize> {
        staging::sweep(&self.data_dir, self.now_ms())
    }

    /// Remove stale temp files (`.nd-*.tmp`, older than one hour) from every account's
    /// `files/`, under each store lock. Blocking. The server runs it on start and every hour
    /// (not on every request: it walks the whole tree).
    pub fn sweep_temps(&self) -> Result<usize> {
        crate::store::sweep_all_temps(&self.data_dir)
    }

    /// Prune the recycle bin of every account by the `recycle_keep_*` settings, journaled with
    /// origin `retention`. Blocking. The offset for local days is read at each run (UTC, with a
    /// warning, when it can't be read). Returns how many old versions were removed and the
    /// problems to report.
    pub fn prune_recycle_all(&self) -> Result<crate::store::PruneAllReport> {
        let offset = crate::localtime::local_utc_offset().unwrap_or_else(|| {
            eprintln!(
                "notedown cloud: can't read the local UTC offset; recycle days are counted in UTC"
            );
            time::UtcOffset::UTC
        });
        crate::store::prune_all_recycle(
            &self.data_dir,
            &self.journal,
            &self.config.retention(),
            self.now_ms(),
            offset,
        )
    }

    /// The prune of the hourly sweep: `None` when `recycle_prune` is off, else
    /// [`AppState::prune_recycle_all`]. Blocking.
    pub fn hourly_recycle_prune(&self) -> Option<Result<crate::store::PruneAllReport>> {
        self.config.recycle_prune.then(|| self.prune_recycle_all())
    }

    /// [`Store::scan`] under `guard` with the server's journal and clock. A plan that holds
    /// is `Error::ScanHold`, so nothing is listed or written from the emptied tree. When this
    /// scan wrote the hold marker (none was there, or one that can't be read), the warning of
    /// [`scan_hold_warning`] goes to stderr once. Blocking.
    pub fn run_scan(
        &self,
        store: &Store,
        guard: &WriteGuard,
        opts: &ScanOptions,
    ) -> Result<ScanReport> {
        self.run_scan_walked(store, guard, opts)
            .map(|(report, _)| report)
    }

    /// [`AppState::run_scan`], plus the scan's walk for a listing of the same scope under the
    /// same guard ([`Store::list_walked`]). Blocking.
    pub(crate) fn run_scan_walked(
        &self,
        store: &Store,
        guard: &WriteGuard,
        opts: &ScanOptions,
    ) -> Result<(ScanReport, Option<Walk>)> {
        let before = store.scan_hold();
        let (report, walk) = store.scan_walked(guard, &self.journal, opts, self.now_ms())?;
        if report.held.is_some() {
            if !matches!(before, Ok(Some(_)))
                && let Ok(Some(hold)) = store.scan_hold()
            {
                eprintln!(
                    "notedown cloud: warning: {}",
                    scan_hold_warning(&self.data_dir, store.slug(), &hold)
                );
            }
            return Err(Error::ScanHold(store.slug().to_string()));
        }
        if report.released {
            match before {
                Ok(Some(_)) => eprintln!(
                    "notedown cloud: account {}: the files are back, device sync goes on",
                    store.slug()
                ),
                _ => eprintln!(
                    "notedown cloud: account {}: removed a scan hold file that couldn't be read, \
                     device sync goes on",
                    store.slug()
                ),
            }
        }
        Ok((report, walk))
    }

    /// Print the warning of [`scan_hold_warning`] for every account that is held already (from
    /// an earlier run), and a line for a hold file that can't be read. `serve` calls it once at
    /// start: a scan prints the warning only when it writes the hold. Blocking.
    pub fn warn_existing_holds(&self) {
        let accounts = match self.auth.accounts().list() {
            Ok(a) => a,
            Err(e) => {
                eprintln!("notedown cloud: scan hold: {e}");
                return;
            }
        };
        for account in accounts {
            let slug = &account.slug;
            let path = self
                .auth
                .accounts()
                .account_dir(slug)
                .join(crate::store::SCAN_HOLD_FILE);
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    eprintln!(
                        "notedown cloud: warning: account {slug}: can't read {}: {e}. Device \
                         sync is on hold.",
                        path.display()
                    );
                    continue;
                }
            };
            match serde_json::from_slice::<ScanHold>(&bytes) {
                Ok(hold) => eprintln!(
                    "notedown cloud: warning: {}",
                    scan_hold_warning(&self.data_dir, slug, &hold)
                ),
                Err(e) => eprintln!(
                    "notedown cloud: warning: account {slug}: can't read {}: {e}. Device sync \
                     is on hold.",
                    path.display()
                ),
            }
        }
    }

    /// One timer pass: a whole-tree scan of every account ([`AppState::scan_account`]). Errors
    /// are logged and never stop the pass. Blocking.
    pub fn scan_all(&self) {
        let accounts = match self.auth.accounts().list() {
            Ok(a) => a,
            Err(e) => {
                eprintln!("notedown cloud: scan: {e}");
                return;
            }
        };
        for account in accounts {
            self.scan_account_of(&account, &Scope::Whole);
        }
    }

    /// Scan `scope` of account `slug` for changes made by hand ([`AppState::run_scan`],
    /// recursive, under the store lock). An account with a scan hold is scanned whole. Skipped
    /// when the account or its `files/` folder does not exist or its protect gate is closed;
    /// busy while a device holds its sync lock. Errors are logged. Blocking.
    pub fn scan_account(&self, slug: &str, scope: &Scope) -> ScanOutcome {
        match self.auth.accounts().get(slug) {
            Ok(Some(account)) => self.scan_account_of(&account, scope),
            Ok(None) => ScanOutcome::Skipped,
            Err(e) => {
                eprintln!("notedown cloud: account {slug}: scan failed: {e}");
                ScanOutcome::Skipped
            }
        }
    }

    fn scan_account_of(&self, account: &crate::auth::Account, scope: &Scope) -> ScanOutcome {
        let slug = &account.slug;
        let files = self
            .auth
            .accounts()
            .account_dir(slug)
            .join(crate::store::FILES_DIR);
        if !files.is_dir() || (self.config.protect && account.imported.is_none()) {
            return ScanOutcome::Skipped;
        }
        if self.sync_locks.holder(slug, self.now_ms()).is_some() {
            return ScanOutcome::Busy;
        }
        let store = match self.store(slug) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("notedown cloud: account {slug}: scan failed: {e}");
                return ScanOutcome::Done;
            }
        };
        let guard = match store.lock_recovered(&self.journal) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("notedown cloud: account {slug}: scan failed: {e}");
                return ScanOutcome::Done;
            }
        };
        // A device may have started its sync, or an import may have closed the protect gate,
        // while this scan waited for the store lock.
        if self.sync_locks.holder(slug, self.now_ms()).is_some() {
            return ScanOutcome::Busy;
        }
        match crate::api::files::protect_locked(self, slug) {
            Ok(()) => {}
            Err(Error::GateClosed(_)) => return ScanOutcome::Skipped,
            Err(e) => {
                eprintln!("notedown cloud: account {slug}: scan failed: {e}");
                return ScanOutcome::Done;
            }
        }
        // Only a whole-tree scan can end a scan hold, so a held account (or one whose hold
        // file can't be read) is scanned whole: files put back into one folder by hand end the
        // hold within seconds.
        let scope = match scope {
            Scope::Folder(_) if !matches!(store.scan_hold(), Ok(None)) => &Scope::Whole,
            s => s,
        };
        let opts = match scope {
            Scope::Whole => ScanOptions::all(self.config.scan_min_files),
            Scope::Folder(folder) => ScanOptions {
                scope: folder.clone(),
                recursive: true,
                dry_run: false,
                accept: false,
                min_files: self.config.scan_min_files,
            },
        };
        match self.run_scan(&store, &guard, &opts) {
            Ok(_) | Err(Error::ScanHold(_)) => {}
            Err(e) => eprintln!("notedown cloud: account {slug}: scan failed: {e}"),
        }
        ScanOutcome::Done
    }

    /// Milliseconds since the epoch, from the clock.
    pub fn now_ms(&self) -> u64 {
        now_ms_of(self.clock.as_ref())
    }
}

/// The warning `serve` prints once when a scan of account `slug` (data folder `data`) holds.
pub fn scan_hold_warning(data: &Path, slug: &str, hold: &ScanHold) -> String {
    let gone = if hold.gone == 1 {
        "1 file is".to_string()
    } else {
        format!("{} files are", hold.gone)
    };
    format!(
        "account {slug}: {gone} gone from the data folder and only {} would be left. \
         Device sync is on hold. If that's on purpose, run: {}",
        hold.left,
        accept_command(data, slug)
    )
}

/// The command that accepts the hold of account `slug`: `notedown cloud scan --data <dir>
/// --account <slug> --accept`, with the data folder as an absolute path (quoted for a shell
/// when it needs it), so it can be copied as it is.
pub fn accept_command(data: &Path, slug: &str) -> String {
    let data = std::path::absolute(data).unwrap_or_else(|_| data.to_path_buf());
    format!(
        "notedown cloud scan --data {} --account {slug} --accept",
        shell_word(&data.display().to_string())
    )
}

/// `text` as one shell word: as it is when it holds only safe characters, else in single
/// quotes.
fn shell_word(text: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "/._-+,:=@%~".contains(c);
    if !text.is_empty() && text.chars().all(safe) {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// `Store::open` warned on stderr when the file system changes names; the protocol log gets a
/// marker too.
fn log_names_not_preserved(log: &ProtocolLog, store: &Store) {
    if !store.names_preserved() {
        let mut m = LogEntry::new(LogKind::Marker);
        m.flags = vec![FLAG_NAMES_NOT_PRESERVED.to_string()];
        m.message = Some(store.names_warning());
        log.record(m);
    }
}

fn report_temp_sweep(r: Result<usize>) {
    match r {
        Ok(0) => {}
        Ok(n) => eprintln!("notedown cloud: removed {n} stale temp files"),
        Err(e) => eprintln!("notedown cloud: temp file sweep failed: {e}"),
    }
}

fn now_ms_of(clock: &dyn Clock) -> u64 {
    let ns = clock.now().unix_timestamp_nanos();
    u64::try_from(ns / 1_000_000).unwrap_or(0)
}

/// Bind every address of `config.listen`.
pub async fn bind(config: &Config) -> Result<Vec<TcpListener>> {
    let mut listeners = Vec::new();
    for addr in &config.listen {
        let l = TcpListener::bind(addr)
            .await
            .map_err(|source| Error::Bind {
                addr: *addr,
                source,
            })?;
        listeners.push(l);
    }
    Ok(listeners)
}

/// Bind every address of `config.listen` and serve until a listener fails.
pub async fn serve(config: Config, data_dir: &Path) -> Result<()> {
    let listeners = bind(&config).await?;
    let state = AppState::open(config, data_dir)?;
    serve_listeners(listeners, state).await
}

/// Serve on listeners that are already bound (tests bind `127.0.0.1:0`).
pub async fn serve_listeners(listeners: Vec<TcpListener>, state: Arc<AppState>) -> Result<()> {
    serve_until(listeners, state, std::future::pending()).await
}

/// Aborts a background task when dropped (the hourly sweep, the scan timer, the push loop, the
/// lock keeper of a request).
pub(crate) struct AbortOnDrop(pub(crate) tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// How long open connections (a device keeps its socket open) get to finish after `shutdown`.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Serve until `shutdown` completes (see [`serve_until_with`]), without extra routes.
pub async fn serve_until(
    listeners: Vec<TcpListener>,
    state: Arc<AppState>,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    serve_until_with(listeners, state, axum::Router::new(), shutdown).await
}

/// Serve until `shutdown` completes, then stop accepting, give open requests a short grace and
/// return. `extra` routes are served in front of the device routes (the MCP endpoint). Every log
/// line is flushed when it is written, so nothing is pending afterwards.
pub async fn serve_until_with(
    listeners: Vec<TcpListener>,
    state: Arc<AppState>,
    extra: axum::Router,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let app = crate::api::router_with(state.clone(), extra);
    // The journal follower and the push tick (D8, D11); stopped when this returns. Its start
    // position is taken before the listeners serve.
    let follower = crate::push::Follower::start(state.clone()).await;
    let _pusher = AbortOnDrop(tokio::spawn(follower.run()));
    // The scan of the data folder for changes made by hand: the first pass right now (after
    // the startup sweeps of `AppState::open`), then every `scan_interval` seconds; stopped when
    // this returns. Started after the follower, so the changes it finds are pushed.
    let st = state.clone();
    if let Err(e) = tokio::task::spawn_blocking(move || st.warn_existing_holds()).await {
        eprintln!("notedown cloud: scan hold: {e}");
    }
    let _scanner = state.config.scan_period().map(|period| {
        let state = state.clone();
        AbortOnDrop(tokio::spawn(async move {
            let mut every = tokio::time::interval(period);
            every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                every.tick().await;
                let st = state.clone();
                if let Err(e) = tokio::task::spawn_blocking(move || st.scan_all()).await {
                    eprintln!("notedown cloud: scan failed: {e}");
                }
            }
        }))
    });
    // The PDFs of the notes in render_dir (when set); stopped when this returns.
    let _renderer = state.config.render_root(&state.data_dir).map(|root| {
        AbortOnDrop(tokio::spawn(crate::render::follow::run(
            state.clone(),
            root,
            crate::render::follow::Timing::DEFAULT,
        )))
    });
    // Folders changed by hand are scanned 2 s after the last change; the timer stays as a
    // backstop. Stopped when this returns.
    let _watcher = state.config.watch.then(|| {
        AbortOnDrop(tokio::spawn(crate::watch::run(
            state.clone(),
            crate::watch::Timing::DEFAULT,
        )))
    });
    // The hourly staging and temp file sweep (the first one ran at start) and the recycle bin
    // prune (its first run is the first tick); stopped when this returns.
    let _sweeper = AbortOnDrop(tokio::spawn(async move {
        let mut every = tokio::time::interval(crate::sync::staging::SWEEP_INTERVAL);
        every.tick().await;
        loop {
            every.tick().await;
            let st = state.clone();
            match tokio::task::spawn_blocking(move || st.sweep_staging()).await {
                Ok(Ok(0)) => {}
                Ok(Ok(n)) => eprintln!("notedown cloud: removed {n} stale upload staging entries"),
                Ok(Err(e)) => eprintln!("notedown cloud: staging sweep failed: {e}"),
                Err(e) => eprintln!("notedown cloud: staging sweep failed: {e}"),
            }
            let st = state.clone();
            match tokio::task::spawn_blocking(move || st.sweep_temps()).await {
                Ok(r) => report_temp_sweep(r),
                Err(e) => eprintln!("notedown cloud: temp file sweep failed: {e}"),
            }
            // The first prune is this first tick, an hour after start, so a dry run with a new
            // binary can come first.
            let st = state.clone();
            match tokio::task::spawn_blocking(move || st.hourly_recycle_prune()).await {
                Ok(None) => {}
                Ok(Some(Ok(report))) => {
                    for p in &report.problems {
                        eprintln!("notedown cloud: {p}");
                    }
                    if report.removed > 0 {
                        eprintln!(
                            "notedown cloud: removed {} old versions from the recycle bin",
                            report.removed
                        );
                    }
                }
                Ok(Some(Err(e))) => eprintln!("notedown cloud: recycle bin prune failed: {e}"),
                Err(e) => eprintln!("notedown cloud: recycle bin prune failed: {e}"),
            }
        }
    }));
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut set = tokio::task::JoinSet::new();
    for l in listeners {
        let addr = l.local_addr().ok();
        let app = app.clone();
        let mut stop = stop_rx.clone();
        set.spawn(async move {
            let r = axum::serve(l, app.into_make_service_with_connect_info::<SocketAddr>())
                .with_graceful_shutdown(async move {
                    let _ = stop.wait_for(|s| *s).await;
                })
                .await;
            (addr, r)
        });
    }
    let mut shutdown = std::pin::pin!(shutdown);
    let mut stopping = false;
    loop {
        let joined = if stopping {
            match tokio::time::timeout(SHUTDOWN_GRACE, set.join_next()).await {
                Ok(j) => j,
                Err(_) => {
                    set.abort_all();
                    return Ok(());
                }
            }
        } else {
            tokio::select! {
                j = set.join_next() => j,
                () = &mut shutdown => {
                    stopping = true;
                    let _ = stop_tx.send(true);
                    continue;
                }
            }
        };
        match joined {
            None => return Ok(()),
            Some(Ok((_, Ok(())))) => {}
            Some(Ok((addr, Err(source)))) => {
                return Err(Error::Serve {
                    addr: addr.map(|a| a.to_string()).unwrap_or_default(),
                    source,
                });
            }
            Some(Err(e)) => {
                return Err(Error::Serve {
                    addr: String::new(),
                    source: std::io::Error::other(e.to_string()),
                });
            }
        }
    }
}
