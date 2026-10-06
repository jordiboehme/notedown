//! In-memory limits: requests per client address, and the retry detector of the request log.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;

/// Requests per address and endpoint within [`WINDOW_MS`].
pub const MAX_PER_WINDOW: usize = 10;
pub const WINDOW_MS: u64 = 60_000;
/// A request repeated within this time after an error is flagged `retry_suspected`.
pub const RETRY_WINDOW_MS: u64 = 5_000;
/// Above this many keys a map is pruned of stale entries on every call.
const PRUNE_ABOVE: usize = 1024;

/// Sliding window per (client address, endpoint). Only allowed requests are counted, so a
/// refused client is free again one minute after its last allowed request. The account lock is
/// not touched here.
#[derive(Debug, Default)]
pub struct RateLimiter {
    windows: Mutex<HashMap<(IpAddr, &'static str), VecDeque<u64>>>,
}

impl RateLimiter {
    pub fn allow(&self, ip: IpAddr, endpoint: &'static str, now_ms: u64) -> bool {
        let mut map = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        let fresh = |t: &u64| now_ms.saturating_sub(*t) < WINDOW_MS;
        if map.len() > PRUNE_ABOVE {
            map.retain(|_, q| {
                q.retain(fresh);
                !q.is_empty()
            });
        }
        let q = map.entry((ip, endpoint)).or_default();
        q.retain(fresh);
        if q.len() >= MAX_PER_WINDOW {
            return false;
        }
        q.push_back(now_ms);
        true
    }
}

/// Remembers the time of the last error per method + path + device (or client address).
#[derive(Debug, Default)]
pub struct RetryTracker {
    last_error: Mutex<HashMap<String, u64>>,
}

impl RetryTracker {
    /// Record the outcome of one request; returns whether it repeats a request that failed
    /// within [`RETRY_WINDOW_MS`].
    pub fn observe(&self, key: String, now_ms: u64, failed: bool) -> bool {
        let mut map = self.last_error.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() > PRUNE_ABOVE {
            map.retain(|_, t| now_ms.saturating_sub(*t) <= RETRY_WINDOW_MS);
        }
        let retry = map
            .get(&key)
            .is_some_and(|t| now_ms.saturating_sub(*t) <= RETRY_WINDOW_MS);
        if failed {
            map.insert(key, now_ms);
        } else {
            map.remove(&key);
        }
        retry
    }
}
