//! The live socket sessions, one per (account slug, device serial) (D10).
//!
//! A session registers after its handshake and takes the receiver of a small channel. File
//! pushes go into the session's queue ([`SocketRegistry::queue`]); the session sends them right
//! after the reply to its next `ratta_ping` or `status` ([`SocketRegistry::deliver`]), merged
//! into one `ServerMessage`, and sends them again until the device answers `ClientMessage
//! "Received"` (`docs/sync-protocol.md` 4.4). The queue lives in the session's entry: it ends
//! with the session, and a session that replaces it starts empty. Everything here is synchronous
//! and never blocks, so the push planner tick can use it from blocking code. The registry holds
//! no clock: the caller passes the time in milliseconds.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::push::{PushItem, file_syn_message, server_message_frame};

/// A session that sent no frame for longer than this counts as not connected: three heartbeats
/// of 5 s (protocol notes 4.3). A device that went to sleep leaves a half-open connection
/// behind, and a push into it would be lost, so it stays pending for the next session.
pub const LIVENESS_MS: u64 = 15_000;

/// A sent push that got no `ClientMessage "Received"` goes out again at the first heartbeat at
/// least this long after the last send (4.4).
pub const RESEND_MS: u64 = 5000;

/// The most items a session's queue keeps; the oldest go first. A device that never answers
/// `Received` cannot make it grow without end, and a lost item is only a lost hint (4.6).
const MAX_QUEUED_ITEMS: usize = 1000;

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

/// The file pushes of one session.
#[derive(Default)]
struct Queue {
    /// Items not sent yet.
    new: Vec<PushItem>,
    /// Items sent and not yet acknowledged with `Received`.
    sent: Vec<PushItem>,
    /// When `sent` went out.
    sent_ms: u64,
}

impl Queue {
    fn push(&mut self, items: Vec<PushItem>) {
        self.new.extend(items);
        let total = self.sent.len() + self.new.len();
        if total > MAX_QUEUED_ITEMS {
            let mut excess = total - MAX_QUEUED_ITEMS;
            let from_sent = excess.min(self.sent.len());
            self.sent.drain(..from_sent);
            excess -= from_sent;
            self.new.drain(..excess);
        }
    }

    /// The items to send at `now_ms`: the unacknowledged ones and the new ones, with only the
    /// newest `STARTSYNC`. `None` when there is nothing, or when the last send is less than
    /// [`RESEND_MS`] ago and not acknowledged (the new items wait for the next heartbeat).
    fn take(&mut self, now_ms: u64) -> Option<&[PushItem]> {
        if !self.sent.is_empty() && now_ms.saturating_sub(self.sent_ms) < RESEND_MS {
            return None;
        }
        if self.sent.is_empty() && self.new.is_empty() {
            return None;
        }
        self.sent.append(&mut self.new);
        if let Some(last) = self.sent.iter().rposition(PushItem::is_startsync) {
            let mut i = 0;
            self.sent.retain(|item| {
                let keep = !item.is_startsync() || i == last;
                i += 1;
                keep
            });
        }
        self.sent_ms = now_ms;
        Some(&self.sent)
    }
}

struct Entry {
    sid: String,
    tx: mpsc::Sender<Outbound>,
    last_inbound: LastInbound,
    queue: Queue,
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
            queue: Queue::default(),
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

    /// Put `items` into the queue of the live session at `now_ms` (same liveness rule as
    /// [`SocketRegistry::send`]). `false` when there is none. Never blocks.
    pub fn queue(&self, slug: &str, serial: &str, items: Vec<PushItem>, now_ms: u64) -> bool {
        let mut map = self.lock();
        match map.get_mut(&(slug.to_string(), serial.to_string())) {
            Some(e) if e.is_live(now_ms) => {
                e.queue.push(items);
                true
            }
            _ => false,
        }
    }

    /// The `ServerMessage` frame that session `sid` sends right after the reply to its
    /// `ratta_ping` or `status` at `now_ms`: every queued item in one message, sent at `now_ms`.
    /// `None` when there is nothing to send now, or when `sid` is not the registered session.
    pub fn deliver(&self, slug: &str, serial: &str, sid: &str, now_ms: u64) -> Option<String> {
        let mut map = self.lock();
        let e = map
            .get_mut(&(slug.to_string(), serial.to_string()))
            .filter(|e| e.sid == sid)?;
        let items = e.queue.take(now_ms)?;
        Some(server_message_frame(&file_syn_message(items, now_ms)))
    }

    /// Session `sid` got `ClientMessage "Received"`: the sent items are delivered.
    pub fn received(&self, slug: &str, serial: &str, sid: &str) {
        let mut map = self.lock();
        if let Some(e) = map
            .get_mut(&(slug.to_string(), serial.to_string()))
            .filter(|e| e.sid == sid)
        {
            e.queue.sent.clear();
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
    use crate::push::startsync_item;

    fn item(n: u64) -> PushItem {
        PushItem::new(&serde_json::json!({ "n": n }), false)
    }

    /// The items of a delivered frame as JSON text, and its send time.
    fn delivered(frame: &str) -> (Vec<String>, u64) {
        let outer: serde_json::Value =
            serde_json::from_str(frame.strip_prefix("42").unwrap()).unwrap();
        assert_eq!(outer[0], "ServerMessage");
        let m: serde_json::Value = serde_json::from_str(outer[1].as_str().unwrap()).unwrap();
        let items = m["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i.to_string())
            .collect();
        (items, m["timestamp"].as_u64().unwrap())
    }

    /// The items as [`delivered`] gives them (parsed, so with sorted keys).
    fn texts(items: &[PushItem]) -> Vec<String> {
        items
            .iter()
            .map(|i| {
                serde_json::from_str::<serde_json::Value>(i.json())
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn queued_items_wait_for_the_heartbeat_and_go_out_merged() {
        let r = SocketRegistry::new();
        let mut rx = r.register("jordi", "SN1", "a", 0).0;
        assert!(r.queue("jordi", "SN1", vec![item(1)], 0));
        assert!(r.queue("jordi", "SN1", vec![item(2)], 100));
        // Nothing goes through the channel: the session sends the queue after a heartbeat.
        assert!(rx.try_recv().is_err());
        let frame = r.deliver("jordi", "SN1", "a", 700).unwrap();
        assert_eq!(delivered(&frame), (texts(&[item(1), item(2)]), 700));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn an_empty_queue_sends_nothing() {
        let r = SocketRegistry::new();
        let _rx = r.register("jordi", "SN1", "a", 0).0;
        assert_eq!(r.deliver("jordi", "SN1", "a", 0), None);
        assert_eq!(r.deliver("jordi", "SN2", "a", 0), None);
    }

    #[test]
    fn without_received_the_items_go_out_again_after_5_s() {
        let r = SocketRegistry::new();
        let _rx = r.register("jordi", "SN1", "a", 0).0;
        assert!(r.queue("jordi", "SN1", vec![item(1)], 0));
        assert!(r.deliver("jordi", "SN1", "a", 1000).is_some());
        assert_eq!(r.deliver("jordi", "SN1", "a", 2000), None);
        // A new item within the 5 s waits too.
        assert!(r.queue("jordi", "SN1", vec![item(2)], 3000));
        assert_eq!(r.deliver("jordi", "SN1", "a", 1000 + RESEND_MS - 1), None);
        let again = r.deliver("jordi", "SN1", "a", 1000 + RESEND_MS).unwrap();
        assert_eq!(
            delivered(&again),
            (texts(&[item(1), item(2)]), 1000 + RESEND_MS)
        );
        r.received("jordi", "SN1", "a");
        assert_eq!(r.deliver("jordi", "SN1", "a", 20_000), None);
    }

    #[test]
    fn received_before_the_window_sends_nothing_twice() {
        let r = SocketRegistry::new();
        let _rx = r.register("jordi", "SN1", "a", 0).0;
        assert!(r.queue("jordi", "SN1", vec![item(1)], 0));
        assert!(r.deliver("jordi", "SN1", "a", 0).is_some());
        r.received("jordi", "SN1", "a");
        assert!(r.queue("jordi", "SN1", vec![item(2)], 500));
        // The new item goes out at once, without the acknowledged one.
        let frame = r.deliver("jordi", "SN1", "a", 1000).unwrap();
        assert_eq!(delivered(&frame).0, texts(&[item(2)]));
        r.received("jordi", "SN1", "a");
        assert_eq!(r.deliver("jordi", "SN1", "a", 10_000), None);
    }

    #[test]
    fn startsync_items_merge_into_one() {
        let r = SocketRegistry::new();
        let _rx = r.register("jordi", "SN1", "a", 0).0;
        assert!(r.queue("jordi", "SN1", vec![startsync_item(None, 1)], 1));
        assert!(r.deliver("jordi", "SN1", "a", 2).is_some());
        assert!(r.queue("jordi", "SN1", vec![startsync_item(None, 3)], 3));
        assert!(r.queue("jordi", "SN1", vec![startsync_item(None, 4)], 4));
        let frame = r.deliver("jordi", "SN1", "a", 2 + RESEND_MS).unwrap();
        assert_eq!(delivered(&frame).0, texts(&[startsync_item(None, 4)]));
    }

    #[test]
    fn the_queue_keeps_the_newest_items_up_to_its_cap() {
        let r = SocketRegistry::new();
        let _rx = r.register("jordi", "SN1", "a", 0).0;
        assert!(r.queue("jordi", "SN1", vec![item(0)], 0));
        assert!(r.deliver("jordi", "SN1", "a", 0).is_some());
        let more: Vec<PushItem> = (1..=MAX_QUEUED_ITEMS as u64).map(item).collect();
        assert!(r.queue("jordi", "SN1", more.clone(), 0));
        let frame = r.deliver("jordi", "SN1", "a", RESEND_MS).unwrap();
        assert_eq!(delivered(&frame).0, texts(&more));
    }

    #[test]
    fn a_silent_or_missing_session_gets_no_queue() {
        let r = SocketRegistry::new();
        let _rx = r.register("jordi", "SN1", "a", 0).0;
        assert!(!r.queue("jordi", "SN1", vec![item(1)], LIVENESS_MS + 1));
        assert!(!r.queue("jordi", "SN2", vec![item(1)], 0));
        assert_eq!(r.deliver("jordi", "SN1", "a", LIVENESS_MS + 1), None);
    }

    #[test]
    fn a_replaced_session_leaves_its_queue_behind() {
        let r = SocketRegistry::new();
        let _first = r.register("jordi", "SN1", "a", 0).0;
        assert!(r.queue("jordi", "SN1", vec![item(1)], 0));
        let _second = r.register("jordi", "SN1", "b", 10).0;
        assert_eq!(r.deliver("jordi", "SN1", "b", 20), None);
        assert_eq!(r.deliver("jordi", "SN1", "a", 20), None);
    }

    #[test]
    fn an_ended_session_drops_its_queue() {
        let r = SocketRegistry::new();
        let _first = r.register("jordi", "SN1", "a", 0).0;
        assert!(r.queue("jordi", "SN1", vec![item(1)], 0));
        r.unregister("jordi", "SN1", "a");
        let _again = r.register("jordi", "SN1", "a", 10).0;
        assert_eq!(r.deliver("jordi", "SN1", "a", 20), None);
    }

    #[test]
    fn an_old_session_cannot_take_or_clear_the_new_queue() {
        let r = SocketRegistry::new();
        let _first = r.register("jordi", "SN1", "a", 0).0;
        let _second = r.register("jordi", "SN1", "b", 0).0;
        assert!(r.queue("jordi", "SN1", vec![item(1)], 0));
        assert_eq!(r.deliver("jordi", "SN1", "a", 0), None);
        assert!(r.deliver("jordi", "SN1", "b", 0).is_some());
        // Received of the old session does not clear the new one's sent items.
        r.received("jordi", "SN1", "a");
        let again = r.deliver("jordi", "SN1", "b", RESEND_MS).unwrap();
        assert_eq!(delivered(&again).0, texts(&[item(1)]));
    }

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
