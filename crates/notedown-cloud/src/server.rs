//! The capture server: state, router and listeners.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::net::TcpListener;

use crate::api::limit::{RateLimiter, RetryTracker};
use crate::auth::Auth;
use crate::diag::{Clock, ProtocolLog, SystemClock};
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
    pub(crate) limiter: RateLimiter,
    pub(crate) retries: RetryTracker,
}

impl AppState {
    pub fn open(config: Config, data_dir: &Path) -> Result<Arc<AppState>> {
        Self::open_with_clock(config, data_dir, Arc::new(SystemClock))
    }

    /// With an injected clock (tests): it drives login times, limits and the log.
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
        Ok(Arc::new(AppState {
            auth: Auth::open(data_dir)?,
            log,
            clock,
            data_dir: data_dir.to_path_buf(),
            config,
            limiter: RateLimiter::default(),
            retries: RetryTracker::default(),
        }))
    }

    /// Milliseconds since the epoch, from the clock.
    pub fn now_ms(&self) -> u64 {
        let ns = self.clock.now().unix_timestamp_nanos();
        u64::try_from(ns / 1_000_000).unwrap_or(0)
    }
}

/// Bind every address of `config.listen` and serve until a listener fails.
pub async fn serve(config: Config, data_dir: &Path) -> Result<()> {
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
    let state = AppState::open(config, data_dir)?;
    serve_listeners(listeners, state).await
}

/// Serve on listeners that are already bound (tests bind `127.0.0.1:0`).
pub async fn serve_listeners(listeners: Vec<TcpListener>, state: Arc<AppState>) -> Result<()> {
    let app = router(state);
    let mut set = tokio::task::JoinSet::new();
    for l in listeners {
        let addr = l.local_addr().ok();
        let app = app.clone();
        set.spawn(async move {
            let r = axum::serve(l, app.into_make_service_with_connect_info::<SocketAddr>()).await;
            (addr, r)
        });
    }
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((_, Ok(()))) => {}
            Ok((addr, Err(source))) => {
                return Err(Error::Serve {
                    addr: addr.map(|a| a.to_string()).unwrap_or_default(),
                    source,
                });
            }
            Err(e) => {
                return Err(Error::Serve {
                    addr: String::new(),
                    source: std::io::Error::other(e.to_string()),
                });
            }
        }
    }
    Ok(())
}
