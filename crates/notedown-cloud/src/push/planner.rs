//! The push planner: pure logic, no clock and no I/O.
//!
//! The journal follower feeds it events, the registry of sockets tells which targets are bound,
//! and a tick asks [`PushPlanner::due`] which targets may get a push now. Whether the server
//! may push at all (sync mode, protect gate, sync lock, connection) is the caller's business.

use std::collections::HashMap;

use serde::Serialize;

use crate::journal::{Event, EventBody, Origin};

/// Quiet time after the newest pending event before a push goes out.
pub const QUIET_MS: u64 = 2000;
/// A push goes out at the latest this long after the oldest pending event.
pub const MAX_WAIT_MS: u64 = 10_000;
/// Minimum time between two pushes to one target (Ratta drops pushes closer than 5 s).
pub const MIN_GAP_MS: u64 = 5000;

/// The most events a [`Pending`] keeps for the detail mode.
const MAX_PENDING_EVENTS: usize = 200;

/// The timing rules of the planner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PushTiming {
    pub quiet_ms: u64,
    pub max_wait_ms: u64,
    pub min_gap_ms: u64,
}

impl Default for PushTiming {
    fn default() -> PushTiming {
        PushTiming {
            quiet_ms: QUIET_MS,
            max_wait_ms: MAX_WAIT_MS,
            min_gap_ms: MIN_GAP_MS,
        }
    }
}

/// A push that waits for its time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// When the oldest pending event was seen.
    pub first_ms: u64,
    /// When the newest pending event was seen.
    pub last_ms: u64,
    pub max_seq: u64,
    /// The cause recorded for `equipmentNo`: the device serial of the newest pending event, or
    /// `None` for CLI and scan changes.
    pub cause: Option<String>,
    /// The pending relevant events, oldest first (for the detail mode), capped at 200.
    pub events: Vec<Event>,
}

#[derive(Debug, Default)]
struct Target {
    pending: Option<Pending>,
    covered_seq: u64,
    last_push_ms: Option<u64>,
}

/// Decides which (account, device) gets a push and when. Keys are `(slug, serial)`.
#[derive(Debug)]
pub struct PushPlanner {
    timing: PushTiming,
    targets: HashMap<(String, String), Target>,
}

/// Only file and folder changes matter, and a snapshot is no change.
fn is_relevant(event: &Event) -> bool {
    if event.origin == Origin::Snapshot {
        return false;
    }
    matches!(
        event.body,
        EventBody::FileCreated { .. }
            | EventBody::FileModified { .. }
            | EventBody::FileMoved { .. }
            | EventBody::FileCopied { .. }
            | EventBody::FileDeleted { .. }
            | EventBody::FolderCreated { .. }
            | EventBody::FolderMoved { .. }
            | EventBody::FolderDeleted { .. }
    )
}

impl PushPlanner {
    pub fn new(timing: PushTiming) -> PushPlanner {
        PushPlanner {
            timing,
            targets: HashMap::new(),
        }
    }

    /// Feed one journal event seen at `now_ms`. `targets` are the bound serials of
    /// `event.account`. Page and device events and snapshot events cause nothing, the device
    /// that caused the event is no target of it, and a target whose sync already covers the
    /// event's `seq` is skipped.
    pub fn observe(&mut self, event: &Event, targets: &[String], now_ms: u64) {
        if !is_relevant(event) {
            return;
        }
        for serial in targets {
            if event.device.as_deref() == Some(serial.as_str()) {
                continue;
            }
            let t = self
                .targets
                .entry((event.account.clone(), serial.clone()))
                .or_default();
            if event.seq <= t.covered_seq {
                continue;
            }
            let p = t.pending.get_or_insert_with(|| Pending {
                first_ms: now_ms,
                last_ms: now_ms,
                max_seq: 0,
                cause: None,
                events: Vec::new(),
            });
            p.last_ms = now_ms;
            p.max_seq = p.max_seq.max(event.seq);
            p.cause = event.device.clone();
            if p.events.len() == MAX_PENDING_EVENTS {
                p.events.remove(0);
            }
            p.events.push(event.clone());
        }
    }

    /// A successful `synchronous/start` of `serial` with the journal at `last_seq`: the sync
    /// lists the tree after every event up to `last_seq`, so a pending push those events caused
    /// is dropped.
    pub fn covered(&mut self, slug: &str, serial: &str, last_seq: u64) {
        let t = self
            .targets
            .entry((slug.to_string(), serial.to_string()))
            .or_default();
        t.covered_seq = t.covered_seq.max(last_seq);
        if t.pending
            .as_ref()
            .is_some_and(|p| p.max_seq <= t.covered_seq)
        {
            t.pending = None;
        }
        // A push that stays pending keeps only the events the sync did not see.
        if let Some(p) = &mut t.pending {
            let covered = t.covered_seq;
            p.events.retain(|e| e.seq > covered);
        }
    }

    /// The targets `(slug, serial)`, sorted, whose debounce and rate rules allow a push at
    /// `now_ms`.
    pub fn due(&self, now_ms: u64) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .targets
            .iter()
            .filter(|(_, t)| {
                let Some(p) = &t.pending else { return false };
                let settled = now_ms.saturating_sub(p.last_ms) >= self.timing.quiet_ms
                    || now_ms.saturating_sub(p.first_ms) >= self.timing.max_wait_ms;
                let rested = t
                    .last_push_ms
                    .is_none_or(|l| now_ms.saturating_sub(l) >= self.timing.min_gap_ms);
                settled && rested
            })
            .map(|(k, _)| k.clone())
            .collect();
        out.sort();
        out
    }

    /// The push to `(slug, serial)` was handed over at `now_ms`: the pending state is cleared.
    pub fn sent(&mut self, slug: &str, serial: &str, now_ms: u64) {
        if let Some(t) = self
            .targets
            .get_mut(&(slug.to_string(), serial.to_string()))
        {
            t.pending = None;
            t.last_push_ms = Some(now_ms);
        }
    }

    pub fn pending(&self, slug: &str, serial: &str) -> Option<&Pending> {
        self.targets
            .get(&(slug.to_string(), serial.to_string()))?
            .pending
            .as_ref()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Item<'a> {
    message_type: &'static str,
    equipment_no: &'a str,
    timestamp: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Message<'a> {
    code: &'static str,
    msg: &'static str,
    timestamp: u64,
    msg_type: &'static str,
    data: [Item<'a>; 1],
}

/// The `FILE-SYN` `STARTSYNC` message as the JSON string argument of `ServerMessage`.
/// `cause` is the serial of the device behind the change; `None` gives `null-null`, as the web
/// UI of Ratta does for a change that no device made.
pub fn startsync_message(cause: Option<&str>, now_ms: u64) -> String {
    let m = Message {
        code: "200",
        msg: "",
        timestamp: now_ms,
        msg_type: "FILE-SYN",
        data: [Item {
            message_type: "STARTSYNC",
            equipment_no: cause.unwrap_or("null-null"),
            timestamp: now_ms,
        }],
    };
    serde_json::to_string(&m).expect("message serializes")
}

/// The socket.io frame `42["ServerMessage", <message string>]`.
pub fn server_message_frame(message: &str) -> String {
    format!(
        "42{}",
        serde_json::to_string(&("ServerMessage", message)).expect("frame serializes")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(origin: Origin, device: Option<&str>, seq: u64, body: EventBody) -> Event {
        let mut e = Event::new("acc", origin, device.map(String::from), "op", body);
        e.seq = seq;
        e
    }

    fn created() -> EventBody {
        EventBody::FileCreated {
            id: 1,
            path: "INBOX/a.pdf".into(),
            kind: crate::journal::FileKind::Document,
            md5: "m".into(),
            size: 1,
            pages_changed: None,
        }
    }

    fn modified() -> EventBody {
        EventBody::FileModified {
            id: 1,
            path: "INBOX/a.pdf".into(),
            kind: crate::journal::FileKind::Document,
            md5: "m".into(),
            prev_md5: "p".into(),
            size: 1,
            pages_changed: None,
        }
    }

    fn targets() -> Vec<String> {
        vec!["A".to_string(), "B".to_string()]
    }

    fn planner() -> PushPlanner {
        PushPlanner::new(PushTiming::default())
    }

    fn is_pending(p: &PushPlanner, serial: &str) -> bool {
        p.pending("acc", serial).is_some()
    }

    #[test]
    fn a_cli_change_makes_every_target_pending_without_cause() {
        let mut p = planner();
        p.observe(&ev(Origin::Cli, None, 1, created()), &targets(), 0);
        assert!(is_pending(&p, "A") && is_pending(&p, "B"));
        assert_eq!(p.pending("acc", "A").unwrap().cause, None);
    }

    #[test]
    fn a_device_change_does_not_echo_to_that_device() {
        let mut p = planner();
        p.observe(&ev(Origin::Device, Some("A"), 1, modified()), &targets(), 0);
        assert!(!is_pending(&p, "A"));
        assert!(is_pending(&p, "B"));
        assert_eq!(p.pending("acc", "B").unwrap().cause.as_deref(), Some("A"));
    }

    #[test]
    fn page_device_and_snapshot_events_push_nothing() {
        let mut p = planner();
        let page = EventBody::PageAdded {
            id: 1,
            file_id: "f".into(),
            page_id: "p".into(),
            position: 0,
        };
        p.observe(&ev(Origin::Device, Some("A"), 1, page), &targets(), 0);
        p.observe(
            &ev(Origin::Device, Some("A"), 2, EventBody::DeviceBound),
            &targets(),
            0,
        );
        p.observe(&ev(Origin::Snapshot, None, 3, created()), &targets(), 0);
        assert!(!is_pending(&p, "A") && !is_pending(&p, "B"));
    }

    #[test]
    fn a_sync_start_covers_older_events() {
        let mut p = planner();
        p.covered("acc", "B", 10);
        p.observe(&ev(Origin::Cli, None, 9, created()), &targets(), 0);
        assert!(!is_pending(&p, "B"));
        assert!(is_pending(&p, "A"));
        p.observe(&ev(Origin::Cli, None, 11, created()), &targets(), 0);
        assert!(is_pending(&p, "B"));
    }

    #[test]
    fn covered_drops_a_pending_push_it_covers() {
        let mut p = planner();
        p.observe(&ev(Origin::Cli, None, 8, created()), &targets(), 0);
        p.covered("acc", "B", 10);
        assert!(!is_pending(&p, "B"));
        assert!(is_pending(&p, "A"));
    }

    #[test]
    fn covered_keeps_a_pending_push_with_newer_events() {
        let mut p = planner();
        p.observe(&ev(Origin::Cli, None, 12, created()), &targets(), 0);
        p.covered("acc", "B", 10);
        assert!(is_pending(&p, "B"));
    }

    #[test]
    fn covered_drops_the_seen_events_from_a_pending_push_that_stays() {
        let mut p = planner();
        for seq in [8, 9, 12] {
            p.observe(&ev(Origin::Cli, None, seq, created()), &targets(), 0);
        }
        p.covered("acc", "B", 10);
        let seqs: Vec<u64> = p
            .pending("acc", "B")
            .unwrap()
            .events
            .iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(seqs, [12]);
        // the other target's pending push is untouched
        assert_eq!(p.pending("acc", "A").unwrap().events.len(), 3);
    }

    #[test]
    fn debounce_waits_for_quiet() {
        let mut p = planner();
        p.observe(&ev(Origin::Cli, None, 1, created()), &targets(), 0);
        p.observe(&ev(Origin::Cli, None, 2, created()), &targets(), 1500);
        assert!(p.due(3000).is_empty());
        assert_eq!(
            p.due(3500),
            vec![
                ("acc".to_string(), "A".to_string()),
                ("acc".to_string(), "B".to_string())
            ]
        );
    }

    #[test]
    fn max_wait_overrides_a_steady_stream() {
        let mut p = planner();
        for i in 0..=12u64 {
            p.observe(
                &ev(Origin::Cli, None, i + 1, created()),
                &targets(),
                i * 1000,
            );
        }
        // events every second: the quiet time never passed, the oldest is 10 s old
        assert_eq!(p.due(10_000).len(), 2);
    }

    #[test]
    fn rate_limits_pushes_to_one_target() {
        let mut p = planner();
        p.observe(&ev(Origin::Cli, None, 1, created()), &targets(), 0);
        p.sent("acc", "B", 1000);
        assert!(!is_pending(&p, "B"));
        p.observe(&ev(Origin::Cli, None, 2, created()), &targets(), 1500);
        let due_b =
            |p: &PushPlanner, now| p.due(now).contains(&("acc".to_string(), "B".to_string()));
        assert!(!due_b(&p, 4000));
        assert!(!due_b(&p, 5999));
        assert!(due_b(&p, 6000));
    }

    #[test]
    fn the_cause_is_the_newest_pending_event() {
        let mut p = planner();
        let t = vec!["C".to_string()];
        p.observe(&ev(Origin::Device, Some("A"), 1, modified()), &t, 0);
        p.observe(&ev(Origin::Device, Some("B"), 2, modified()), &t, 10);
        assert_eq!(p.pending("acc", "C").unwrap().cause.as_deref(), Some("B"));
        p.observe(&ev(Origin::Cli, None, 3, created()), &t, 20);
        let pd = p.pending("acc", "C").unwrap();
        assert_eq!(pd.cause, None);
        assert_eq!((pd.first_ms, pd.last_ms, pd.max_seq), (0, 20, 3));
    }

    #[test]
    fn accounts_do_not_mix() {
        let mut p = planner();
        let mut e = ev(Origin::Cli, None, 1, created());
        e.account = "x".into();
        p.observe(&e, &targets(), 0);
        assert!(p.pending("x", "A").is_some());
        assert!(p.pending("y", "A").is_none() && p.pending("acc", "A").is_none());
    }

    #[test]
    fn events_are_capped_at_200_dropping_the_oldest() {
        let mut p = planner();
        let t = vec!["B".to_string()];
        for i in 1..=250u64 {
            p.observe(&ev(Origin::Cli, None, i, created()), &t, i);
        }
        let pd = p.pending("acc", "B").unwrap();
        assert_eq!(pd.events.len(), 200);
        assert_eq!(pd.events[0].seq, 51);
        assert_eq!(pd.events[199].seq, 250);
    }

    #[test]
    fn startsync_message_has_the_fixed_field_order() {
        assert_eq!(
            startsync_message(None, 1791270408879),
            r#"{"code":"200","msg":"","timestamp":1791270408879,"msgType":"FILE-SYN","data":[{"messageType":"STARTSYNC","equipmentNo":"null-null","timestamp":1791270408879}]}"#
        );
        assert!(startsync_message(Some("SN1"), 5).contains(r#""equipmentNo":"SN1""#));
    }

    #[test]
    fn the_frame_wraps_the_message_as_a_string() {
        let m = startsync_message(None, 7);
        let frame = server_message_frame(&m);
        assert!(
            frame.starts_with(r#"42["ServerMessage","{\"code\":\"200\""#),
            "{frame}"
        );
        let v: serde_json::Value = serde_json::from_str(&frame[2..]).unwrap();
        assert_eq!(v[0], "ServerMessage");
        assert_eq!(v[1], m);
    }
}
