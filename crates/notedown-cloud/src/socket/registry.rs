//! The live socket sessions, one per (account slug, device serial) (D10).
//!
//! A session registers after its handshake and takes the receiver of a small channel; the
//! server hands pushes to [`SocketRegistry::send`]. Everything here is synchronous and never
//! blocks, so the push planner tick can use it from blocking code.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::mpsc;

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

/// The registered sessions: (slug, serial) to (session id, channel).
#[derive(Default)]
pub struct SocketRegistry {
    sessions: Mutex<HashMap<Key, (String, mpsc::Sender<Outbound>)>>,
}

impl SocketRegistry {
    pub fn new() -> SocketRegistry {
        SocketRegistry::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Key, (String, mpsc::Sender<Outbound>)>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Register the session `sid`; an older session of the same (slug, serial) gets
    /// `Outbound::Close("replaced")` and is dropped from the map. Returns the receiver.
    pub fn register(&self, slug: &str, serial: &str, sid: &str) -> mpsc::Receiver<Outbound> {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let old = self.lock().insert(
            (slug.to_string(), serial.to_string()),
            (sid.to_string(), tx),
        );
        if let Some((_, old_tx)) = old {
            // A full channel drops the notice; the old session then ends with its connection.
            let _ = old_tx.try_send(Outbound::Close("replaced"));
        }
        rx
    }

    /// Remove the entry only when it still belongs to `sid`.
    pub fn unregister(&self, slug: &str, serial: &str, sid: &str) {
        let mut map = self.lock();
        let key = (slug.to_string(), serial.to_string());
        if map.get(&key).is_some_and(|(s, _)| s == sid) {
            map.remove(&key);
        }
    }

    /// Hand a frame to the live session. `false` when there is none or its channel is closed or
    /// full. Never blocks.
    pub fn send(&self, slug: &str, serial: &str, frame: String) -> bool {
        let map = self.lock();
        match map.get(&(slug.to_string(), serial.to_string())) {
            Some((_, tx)) => tx.try_send(Outbound::Frame(frame)).is_ok(),
            None => false,
        }
    }

    /// Whether a session of this device is registered with an open channel.
    pub fn is_connected(&self, slug: &str, serial: &str) -> bool {
        self.lock()
            .get(&(slug.to_string(), serial.to_string()))
            .is_some_and(|(_, tx)| !tx.is_closed())
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
        let mut rx = r.register("jordi", "SN1", "a");
        assert!(r.is_connected("jordi", "SN1"));
        assert!(r.send("jordi", "SN1", "f".into()));
        assert_eq!(rx.try_recv().unwrap(), frame("f"));
    }

    #[test]
    fn second_register_closes_the_first() {
        let r = SocketRegistry::new();
        let mut first = r.register("jordi", "SN1", "a");
        let mut second = r.register("jordi", "SN1", "b");
        assert_eq!(first.try_recv().unwrap(), Outbound::Close("replaced"));
        assert!(first.try_recv().is_err());
        assert!(r.send("jordi", "SN1", "f".into()));
        assert_eq!(second.try_recv().unwrap(), frame("f"));
        assert!(first.try_recv().is_err());
    }

    #[test]
    fn unregister_with_the_old_sid_keeps_the_new_entry() {
        let r = SocketRegistry::new();
        let _first = r.register("jordi", "SN1", "a");
        let _second = r.register("jordi", "SN1", "b");
        r.unregister("jordi", "SN1", "a");
        assert!(r.is_connected("jordi", "SN1"));
        r.unregister("jordi", "SN1", "b");
        assert!(!r.is_connected("jordi", "SN1"));
    }

    #[test]
    fn send_after_the_receiver_was_dropped_fails() {
        let r = SocketRegistry::new();
        drop(r.register("jordi", "SN1", "a"));
        assert!(!r.send("jordi", "SN1", "f".into()));
        assert!(!r.is_connected("jordi", "SN1"));
    }

    #[test]
    fn send_to_a_full_channel_fails_without_blocking() {
        let r = SocketRegistry::new();
        let _rx = r.register("jordi", "SN1", "a");
        for _ in 0..CHANNEL_CAPACITY {
            assert!(r.send("jordi", "SN1", "f".into()));
        }
        assert!(!r.send("jordi", "SN1", "f".into()));
    }

    #[test]
    fn other_serial_or_slug_is_not_reached() {
        let r = SocketRegistry::new();
        let mut rx = r.register("jordi", "SN1", "a");
        assert!(!r.send("jordi", "SN2", "f".into()));
        assert!(!r.send("anna", "SN1", "f".into()));
        assert!(!r.is_connected("anna", "SN1"));
        assert!(rx.try_recv().is_err());
    }
}
