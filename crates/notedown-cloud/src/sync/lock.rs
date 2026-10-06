//! The sync lock: one sync session per account (`docs/sync-protocol.md` 3.5,
//! `synchronous/start`). It lives in memory only; a restart drops every lock, and the device
//! simply starts its next sync.

use std::collections::HashMap;
use std::sync::Mutex;

/// A lock that is not extended lapses after this time (ms), as Ratta's `lock_cloud_time`.
pub const SYNC_LOCK_TTL_MS: u64 = 30_000;

/// The answer to `synchronous/start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// Nobody held the lock (or it lapsed): the device holds it now.
    Taken,
    /// The device held it already: renewed.
    Renewed,
    /// Another device holds it (`E0078`).
    Busy { owner: String },
}

#[derive(Debug, Clone)]
struct Held {
    owner: String,
    expires_at_ms: u64,
}

/// Per-account lock owners (device serials), keyed by account slug.
#[derive(Debug, Default)]
pub struct SyncLocks {
    held: Mutex<HashMap<String, Held>>,
}

impl SyncLocks {
    pub fn new() -> SyncLocks {
        SyncLocks::default()
    }

    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Held>> {
        self.held.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `synchronous/start` of `serial` for account `slug` at `now_ms`.
    pub fn start(&self, slug: &str, serial: &str, now_ms: u64) -> StartOutcome {
        let mut map = self.map();
        let outcome = match map.get(slug) {
            Some(h) if h.expires_at_ms > now_ms && h.owner != serial => {
                return StartOutcome::Busy {
                    owner: h.owner.clone(),
                };
            }
            Some(h) if h.expires_at_ms > now_ms => StartOutcome::Renewed,
            _ => StartOutcome::Taken,
        };
        map.insert(
            slug.to_string(),
            Held {
                owner: serial.to_string(),
                expires_at_ms: now_ms + SYNC_LOCK_TTL_MS,
            },
        );
        outcome
    }

    /// Extend the lock by [`SYNC_LOCK_TTL_MS`] from `now_ms` when `serial` holds it. Every file
    /// call of the owning device does this, and transfers do it while the bytes flow. Returns
    /// whether the lock was extended. A lapsed lock is not revived here; [`SyncLocks::retake`]
    /// does that for the last holder after the protect gate.
    pub fn touch(&self, slug: &str, serial: &str, now_ms: u64) -> bool {
        let mut map = self.map();
        match map.get_mut(slug) {
            Some(h) if h.owner == serial && h.expires_at_ms > now_ms => {
                h.expires_at_ms = h.expires_at_ms.max(now_ms + SYNC_LOCK_TTL_MS);
                true
            }
            _ => false,
        }
    }

    /// The lock for a file call of `serial` at `now_ms`, after the protect gate passed: the
    /// device that last held the lock keeps it. A live lock of this device is extended; a lapsed
    /// one is taken again silently (a long pause inside a session, such as hashing a large file
    /// on the device). Returns whether `serial` holds the live lock now. Another device's live
    /// lock is never touched, and nothing is taken after `synchronous/end` released the lock or
    /// when another device held it last.
    pub fn retake(&self, slug: &str, serial: &str, now_ms: u64) -> bool {
        let mut map = self.map();
        match map.get_mut(slug) {
            Some(h) if h.owner == serial => {
                h.expires_at_ms = h.expires_at_ms.max(now_ms + SYNC_LOCK_TTL_MS);
                true
            }
            _ => false,
        }
    }

    /// `synchronous/end`: release the lock when `serial` holds it (or it lapsed). Returns whether
    /// a lock of this device was released. Another device's live lock stays.
    pub fn end(&self, slug: &str, serial: &str, now_ms: u64) -> bool {
        let mut map = self.map();
        match map.get(slug) {
            Some(h) if h.owner == serial => {
                map.remove(slug);
                true
            }
            Some(h) if h.expires_at_ms <= now_ms => {
                map.remove(slug);
                false
            }
            _ => false,
        }
    }

    /// The device holding the live lock of `slug`, if any.
    pub fn holder(&self, slug: &str, now_ms: u64) -> Option<String> {
        self.map()
            .get(slug)
            .filter(|h| h.expires_at_ms > now_ms)
            .map(|h| h.owner.clone())
    }
}
