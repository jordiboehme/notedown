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
use crate::store::{Store, WirePath};
use crate::sync::staging::{self, StagingLocks};
use crate::sync::{SyncLocks, UrlSigner};
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
        for (slug, r) in crate::store::recover_all(data_dir, &journal)? {
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
        report_temp_sweep(crate::store::sweep_all_temps(data_dir));
        Ok(Arc::new(AppState {
            auth: Auth::open(data_dir)?,
            url_signer: UrlSigner::load_or_create(data_dir)?,
            sync_locks: SyncLocks::new(),
            stores: Mutex::new(HashMap::new()),
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
        Ok(stores().entry(slug.to_string()).or_insert(store).clone())
    }

    /// The bytes of every file the account's listing shows, reused for [`USAGE_CACHE_MS`].
    /// Blocking: it may list (and hash) the whole tree under the store lock.
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
        let used = store
            .list(&guard, &WirePath::root(), true)?
            .iter()
            .map(|e| e.size)
            .sum();
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

    /// Milliseconds since the epoch, from the clock.
    pub fn now_ms(&self) -> u64 {
        now_ms_of(self.clock.as_ref())
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

/// Aborts a background task when dropped (the hourly sweep, the lock keeper of a request).
pub(crate) struct AbortOnDrop(pub(crate) tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// How long open connections (a device keeps its socket open) get to finish after `shutdown`.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Serve until `shutdown` completes, then stop accepting, give open requests a short grace and
/// return. Every log line is flushed when it is written, so nothing is pending afterwards.
pub async fn serve_until(
    listeners: Vec<TcpListener>,
    state: Arc<AppState>,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let app = router(state.clone());
    // The hourly staging and temp file sweep (the first one ran at start); stopped when this
    // returns.
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
