//! The live socket sessions, one per (account slug, device serial) (D10).
//!
//! A session registers after its handshake and takes the receiver of a small channel; the
//! server hands pushes to [`SocketRegistry::send`]. Everything here is synchronous and never
//! blocks, so the push planner tick can use it from blocking code. The registry holds no clock:
//! the caller passes the time in milliseconds.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

/// A session that sent no frame for longer than this counts as not connected: three heartbeats
/// of 5 s (protocol notes 4.3). A device that went to sleep leaves a half-open connection
/// behind, and a push into it would be lost, so it stays pending for the next session.
pub const LIVENESS_MS: u64 = 15_000;

/// Capacity of a session's channel. A push is a hint, so a full channel is a failed hand-over.
const CHANNEL_CAPACITY: usize = 8;

/// What the server tells a live session to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Outbound {
    /// Send this socket.io frame (already built) and log it.
    Frame(String),
    /// Close the session with `close_code::AWAY` and this reason (D10).
    Close(&'static str),
}

type Key = (String, String);

/// The time of the newest frame from the device, in ms. The session updates it on every
/// received message; registration counts as one.
pub type LastInbound = Arc<AtomicU64>;

struct Entry {
    sid: String,
    tx: mpsc::Sender<Outbound>,
    last_inbound: LastInbound,
}

impl Entry {
    /// Open channel and a frame from the device within [`LIVENESS_MS`] before `now_ms`.
    fn is_live(&self, now_ms: u64) -> bool {
        !self.tx.is_closed()
            && now_ms.saturating_sub(self.last_inbound.load(Ordering::Relaxed)) <= LIVENESS_MS
    }
}

/// The registered sessions: (slug, serial) to (session id, channel, last inbound time).
#[derive(Default)]
pub struct SocketRegistry {
    sessions: Mutex<HashMap<Key, Entry>>,
}

impl SocketRegistry {
    pub fn new() -> SocketRegistry {
        SocketRegistry::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Entry>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Register the session `sid` at `now_ms`; an older session of the same (slug, serial) gets
    /// `Outbound::Close("replaced")` and is dropped from the map. Returns the receiver and the
    /// last inbound time, which the session sets on every frame it receives.
    pub fn register(
        &self,
        slug: &str,
        serial: &str,
        sid: &str,
        now_ms: u64,
    ) -> (mpsc::Receiver<Outbound>, LastInbound) {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let last_inbound = Arc::new(AtomicU64::new(now_ms));
        let entry = Entry {
            sid: sid.to_string(),
            tx,
            last_inbound: last_inbound.clone(),
        };
        let old = self
            .lock()
            .insert((slug.to_string(), serial.to_string()), entry);
        if let Some(old) = old {
            // A full channel drops the notice; the dropped sender then closes the old session.
            let _ = old.tx.try_send(Outbound::Close("replaced"));
        }
        (rx, last_inbound)
    }

    /// Remove the entry only when it still belongs to `sid`.
    pub fn unregister(&self, slug: &str, serial: &str, sid: &str) {
        let mut map = self.lock();
        let key = (slug.to_string(), serial.to_string());
        if map.get(&key).is_some_and(|e| e.sid == sid) {
            map.remove(&key);
        }
    }

    /// Close the live session at `now_ms` with `reason` (`Outbound::Close`): the device connects
    /// again. Same liveness rule as [`SocketRegistry::send`]. Never blocks.
    pub fn close(&self, slug: &str, serial: &str, reason: &'static str, now_ms: u64) -> bool {
        let map = self.lock();
        match map.get(&(slug.to_string(), serial.to_string())) {
            Some(e) if e.is_live(now_ms) => e.tx.try_send(Outbound::Close(reason)).is_ok(),
            _ => false,
        }
    }

    /// Hand a frame to the live session at `now_ms`. `false` when there is none, its channel is
    /// closed or full, or the device sent nothing for longer than [`LIVENESS_MS`]. Never blocks.
    pub fn send(&self, slug: &str, serial: &str, frame: String, now_ms: u64) -> bool {
        let map = self.lock();
        match map.get(&(slug.to_string(), serial.to_string())) {
            Some(e) if e.is_live(now_ms) => e.tx.try_send(Outbound::Frame(frame)).is_ok(),
            _ => false,
        }
    }

    /// Whether a session of this device is registered with an open channel and sent a frame
    /// within [`LIVENESS_MS`] before `now_ms`.
    pub fn is_connected(&self, slug: &str, serial: &str, now_ms: u64) -> bool {
        self.lock()
            .get(&(slug.to_string(), serial.to_string()))
            .is_some_and(|e| e.is_live(now_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(s: &str) -> Outbound {
        Outbound::Frame(s.to_string())
    }

    #[test]
    fn register_then_send_delivers() {
        let r = SocketRegistry::new();
        let mut rx = r.register("jordi", "SN1", "a", 0).0;
        assert!(r.is_connected("jordi", "SN1", 0));
        assert!(r.send("jordi", "SN1", "f".into(), 0));
        assert_eq!(rx.try_recv().unwrap(), frame("f"));
    }

    #[test]
    fn close_reaches_only_a_live_session() {
        let r = SocketRegistry::new();
        let mut rx = r.register("jordi", "SN1", "a", 0).0;
        assert!(!r.close("jordi", "SN1", "sync", LIVENESS_MS + 1));
        assert!(rx.try_recv().is_err());
        assert!(r.close("jordi", "SN1", "sync", LIVENESS_MS));
        assert_eq!(rx.try_recv().unwrap(), Outbound::Close("sync"));
        assert!(!r.close("jordi", "SN2", "sync", 0));
    }

    #[test]
    fn second_register_closes_the_first() {
        let r = SocketRegistry::new();
        let mut first = r.register("jordi", "SN1", "a", 0).0;
        let mut second = r.register("jordi", "SN1", "b", 0).0;
        assert_eq!(first.try_recv().unwrap(), Outbound::Close("replaced"));
        assert!(first.try_recv().is_err());
        assert!(r.send("jordi", "SN1", "f".into(), 0));
        assert_eq!(second.try_recv().unwrap(), frame("f"));
        assert!(first.try_recv().is_err());
    }

    #[test]
    fn replacing_a_session_with_a_full_channel_still_ends_its_receiver() {
        let r = SocketRegistry::new();
        let mut first = r.register("jordi", "SN1", "a", 0).0;
        for _ in 0..CHANNEL_CAPACITY {
            assert!(r.send("jordi", "SN1", "f".into(), 0));
        }
        let _second = r.register("jordi", "SN1", "b", 0).0;
        for _ in 0..CHANNEL_CAPACITY {
            assert_eq!(first.try_recv().unwrap(), frame("f"));
        }
        // The notice did not fit; the dropped sender is what tells the session.
        assert_eq!(
            first.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        );
    }

    #[test]
    fn unregister_with_the_old_sid_keeps_the_new_entry() {
        let r = SocketRegistry::new();
        let _first = r.register("jordi", "SN1", "a", 0).0;
        let _second = r.register("jordi", "SN1", "b", 0).0;
        r.unregister("jordi", "SN1", "a");
        assert!(r.is_connected("jordi", "SN1", 0));
        r.unregister("jordi", "SN1", "b");
        assert!(!r.is_connected("jordi", "SN1", 0));
    }

    #[test]
    fn send_after_the_receiver_was_dropped_fails() {
        let r = SocketRegistry::new();
        drop(r.register("jordi", "SN1", "a", 0).0);
        assert!(!r.send("jordi", "SN1", "f".into(), 0));
        assert!(!r.is_connected("jordi", "SN1", 0));
    }

    #[test]
    fn send_to_a_full_channel_fails_without_blocking() {
        let r = SocketRegistry::new();
        let _rx = r.register("jordi", "SN1", "a", 0).0;
        for _ in 0..CHANNEL_CAPACITY {
            assert!(r.send("jordi", "SN1", "f".into(), 0));
        }
        assert!(!r.send("jordi", "SN1", "f".into(), 0));
    }

    #[test]
    fn other_serial_or_slug_is_not_reached() {
        let r = SocketRegistry::new();
        let mut rx = r.register("jordi", "SN1", "a", 0).0;
        assert!(!r.send("jordi", "SN2", "f".into(), 0));
        assert!(!r.send("anna", "SN1", "f".into(), 0));
        assert!(!r.is_connected("anna", "SN1", 0));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_silent_session_is_not_connected_after_the_liveness_limit() {
        let r = SocketRegistry::new();
        let mut rx = r.register("jordi", "SN1", "a", 1000).0;
        assert!(r.is_connected("jordi", "SN1", 1000 + LIVENESS_MS));
        assert!(!r.is_connected("jordi", "SN1", 1001 + LIVENESS_MS));
        assert!(!r.send("jordi", "SN1", "f".into(), 1001 + LIVENESS_MS));
        // Nothing was handed over: the push stays with the caller.
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn an_inbound_frame_makes_a_silent_session_live_again() {
        let r = SocketRegistry::new();
        let (mut rx, last_inbound) = r.register("jordi", "SN1", "a", 1000);
        let late = 1000 + LIVENESS_MS + 5000;
        assert!(!r.is_connected("jordi", "SN1", late));
        last_inbound.store(late - 100, Ordering::Relaxed);
        assert!(r.is_connected("jordi", "SN1", late));
        assert!(r.send("jordi", "SN1", "f".into(), late));
        assert_eq!(rx.try_recv().unwrap(), frame("f"));
    }

    #[test]
    fn a_new_session_of_a_silent_device_is_live() {
        let r = SocketRegistry::new();
        let _old = r.register("jordi", "SN1", "a", 0);
        let late = LIVENESS_MS + 1;
        assert!(!r.is_connected("jordi", "SN1", late));
        let mut new = r.register("jordi", "SN1", "b", late).0;
        assert!(r.send("jordi", "SN1", "f".into(), late));
        assert_eq!(new.try_recv().unwrap(), frame("f"));
    }
}
