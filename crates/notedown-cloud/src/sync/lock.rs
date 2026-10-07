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

/// The lock for a write of a device ([`SyncLocks::take_for_write`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteLock {
    /// The device held the lock (live, or lapsed and taken again): a write of its session.
    Held,
    /// Nobody held a live lock: the device holds it now without `synchronous/start`. Auto Sync
    /// sends a changed file this way (`docs/sync-protocol.md` 9.3).
    Taken,
    /// Another device holds the live lock.
    Busy,
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

    /// The lock for a write of `serial` at `now_ms`, after the protect gate passed. The device
    /// that held the lock last keeps it, as in [`SyncLocks::retake`]. A device that writes
    /// without a session takes a free or lapsed lock, as if it had sent `synchronous/start`;
    /// another device's live lock is never touched.
    pub fn take_for_write(&self, slug: &str, serial: &str, now_ms: u64) -> WriteLock {
        let mut map = self.map();
        let outcome = match map.get(slug) {
            Some(h) if h.owner == serial => WriteLock::Held,
            Some(h) if h.expires_at_ms > now_ms => return WriteLock::Busy,
            _ => WriteLock::Taken,
        };
        let held = map.entry(slug.to_string()).or_insert(Held {
            owner: serial.to_string(),
            expires_at_ms: 0,
        });
        held.owner = serial.to_string();
        held.expires_at_ms = held.expires_at_ms.max(now_ms + SYNC_LOCK_TTL_MS);
        outcome
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_without_a_session_takes_a_free_lock() {
        let locks = SyncLocks::new();
        assert_eq!(locks.take_for_write("a", "D1", 1000), WriteLock::Taken);
        assert_eq!(locks.holder("a", 1000).as_deref(), Some("D1"));
        assert_eq!(locks.take_for_write("a", "D1", 2000), WriteLock::Held);
        // Another device starting a sync now waits, as for a normal session.
        assert!(matches!(
            locks.start("a", "D2", 3000),
            StartOutcome::Busy { .. }
        ));
    }

    #[test]
    fn a_write_takes_a_lapsed_lock_of_another_device() {
        let locks = SyncLocks::new();
        assert_eq!(locks.start("a", "D2", 0), StartOutcome::Taken);
        assert_eq!(locks.take_for_write("a", "D1", 10_000), WriteLock::Busy);
        assert_eq!(locks.holder("a", 10_000).as_deref(), Some("D2"));
        assert_eq!(
            locks.take_for_write("a", "D1", SYNC_LOCK_TTL_MS + 1),
            WriteLock::Taken
        );
        assert_eq!(
            locks.holder("a", SYNC_LOCK_TTL_MS + 1).as_deref(),
            Some("D1")
        );
    }

    #[test]
    fn a_write_after_the_end_of_a_session_takes_the_lock_again() {
        let locks = SyncLocks::new();
        assert_eq!(locks.start("a", "D1", 0), StartOutcome::Taken);
        assert!(locks.end("a", "D1", 100));
        assert!(!locks.retake("a", "D1", 200));
        assert_eq!(locks.take_for_write("a", "D1", 200), WriteLock::Taken);
        assert_eq!(locks.take_for_write("b", "D1", 200), WriteLock::Taken);
        assert_eq!(locks.holder("b", 200).as_deref(), Some("D1"));
    }
}
