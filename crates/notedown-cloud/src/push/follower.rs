//! The journal follower and the push loop.
//!
//! The follower reads the journal events after its cursor and feeds them to the planner. The
//! cursor lives in memory only: it starts at the journal's `last_seq` when the server starts
//! (D11), so older changes never cause a push, and a restart drops pending pushes (D6). The tick
//! hands every due push that D8 allows to the device's socket session: `reconnect` closes it,
//! `startsync` and `detail` put their items into its queue, which the session sends after its
//! next heartbeat reply (`docs/sync-protocol.md` 4.4).

use std::collections::HashMap;
use std::sync::{Arc, MutexGuard};
use std::time::Duration;

use super::{PushPlanner, detail_items, startsync_item};
use crate::diag::{LogEntry, LogKind};
use crate::server::AppState;
use crate::{PushMode, SyncMode};

/// How often the follower looks at the journal files.
const FOLLOW_INTERVAL: Duration = Duration::from_secs(1);
/// How often the tick looks for due pushes.
const TICK_INTERVAL: Duration = Duration::from_millis(500);

fn planner(state: &AppState) -> MutexGuard<'_, PushPlanner> {
    state.push.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where the follower starts: the journal's `last_seq` (D11). Blocking.
pub fn start_seq(state: &AppState) -> crate::Result<u64> {
    state.journal.last_seq()
}

/// The bound devices of `slug` (D6). A read error gives no targets.
fn bound_targets(state: &AppState, slug: &str) -> Vec<String> {
    match state.auth.devices().list(slug) {
        Ok(devices) => devices
            .into_iter()
            .filter(|d| d.bound)
            .map(|d| d.serial)
            .collect(),
        Err(e) => {
            eprintln!("notedown cloud: push: devices of account {slug}: {e}");
            Vec::new()
        }
    }
}

/// One follower step: read the events after `*last_seq`, feed the planner, advance `*last_seq`.
/// Returns the number of events read. A line that cannot be read is skipped and logged as one
/// marker in the protocol log. Blocking.
pub fn follow_once(state: &AppState, last_seq: &mut u64) -> crate::Result<usize> {
    let now = state.now_ms();
    // The bound devices per account, read once per step.
    let mut targets: HashMap<String, Vec<String>> = HashMap::new();
    let mut read = 0;
    let mut bad: Vec<String> = Vec::new();
    for item in state.journal.read_from(*last_seq + 1)? {
        let event = match item {
            Ok(event) => event,
            Err(e) => {
                bad.push(e.to_string());
                continue;
            }
        };
        let serials = targets
            .entry(event.account.clone())
            .or_insert_with(|| bound_targets(state, &event.account));
        planner(state).observe(&event, serials, now);
        *last_seq = (*last_seq).max(event.seq);
        read += 1;
    }
    if let Some(first) = bad.first() {
        let mut m = LogEntry::new(LogKind::Marker);
        m.message = Some(format!(
            "push follower: skipped {} journal line(s) that cannot be read: {first}",
            bad.len()
        ));
        state.log.record(m);
    }
    Ok(read)
}

/// The close reason of a `reconnect` push.
pub const RECONNECT_REASON: &str = "sync";

/// One tick at `now_ms`: hand over every due push that D8 allows. Returns the pushes handed
/// over. A target that fails a gate keeps its pending push. Blocking (it reads `account.json`).
pub fn tick(state: &AppState, now_ms: u64) -> usize {
    if state.config.sync != SyncMode::Accept || state.config.push == PushMode::Off {
        return 0;
    }
    let due = planner(state).due(now_ms);
    let mut gates: HashMap<String, bool> = HashMap::new();
    let mut sent = 0;
    for (slug, serial) in due {
        let open = *gates
            .entry(slug.clone())
            .or_insert_with(|| gate_open(state, &slug));
        if !open
            || state.sync_locks.holder(&slug, now_ms).is_some()
            || !state.sockets.is_connected(&slug, &serial, now_ms)
        {
            continue;
        }
        // Held from the look at the pending push to `sent`, so a `covered` of a sync start cannot
        // slip in and be wiped. In detail mode the items are built under the lock, and that can
        // read the store's id registry from disk: a sync start and the follower wait for it.
        let mut planner = planner(state);
        let Some(pending) = planner.pending(&slug, &serial) else {
            continue;
        };
        if state.config.push == PushMode::Reconnect {
            // Only the WebSocket is closed, without a socket.io disconnect packet (`41`): a
            // client treats a server disconnect as final and does not connect again.
            if state
                .sockets
                .close(&slug, &serial, RECONNECT_REASON, now_ms)
            {
                planner.sent(&slug, &serial, now_ms);
                sent += 1;
            }
            continue;
        }
        let cause = pending.cause.as_deref();
        // `detail` falls back to `STARTSYNC` when the cap dropped events (the list would be
        // partial) or when its items cannot be built.
        let detail = (state.config.push == PushMode::Detail && !pending.truncated)
            .then(|| {
                let store = state.store(&slug).ok()?;
                detail_items(&pending.events, &store, now_ms).ok()
            })
            .flatten()
            .filter(|items| !items.is_empty());
        let items = detail.unwrap_or_else(|| vec![startsync_item(cause, now_ms)]);
        if state.sockets.queue(&slug, &serial, items, now_ms) {
            planner.sent(&slug, &serial, now_ms);
            sent += 1;
        }
    }
    sent
}

/// The protect gate (D8): with `protect = true` an account gets pushes only after its import.
fn gate_open(state: &AppState, slug: &str) -> bool {
    if !state.config.protect {
        return true;
    }
    match state.auth.accounts().get(slug) {
        Ok(account) => account.is_some_and(|a| a.imported.is_some()),
        Err(e) => {
            eprintln!("notedown cloud: push: account {slug}: {e}");
            false
        }
    }
}

/// The file count and the size of the newest journal file: the follower reads only when this
/// changes (D11).
fn journal_stamp(state: &AppState) -> crate::Result<(usize, u64)> {
    let files = state.journal.files()?;
    let size = match files.last() {
        Some((_, path)) => std::fs::metadata(path).map_err(crate::io_err(path))?.len(),
        None => 0,
    };
    Ok((files.len(), size))
}

/// Run `f` on the blocking pool; `None` when the task failed.
async fn blocking<T: Send + 'static>(
    state: &Arc<AppState>,
    f: impl FnOnce(&AppState) -> T + Send + 'static,
) -> Option<T> {
    let st = state.clone();
    match tokio::task::spawn_blocking(move || f(&st)).await {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("notedown cloud: push: {e}");
            None
        }
    }
}

/// The follower of a running server: its cursor and the journal stamp it belongs to.
pub struct Follower {
    state: Arc<AppState>,
    /// `None` until the start position is known. It is never guessed: a cursor at 0 would push
    /// the whole history.
    cursor: Option<u64>,
    seen: Option<(usize, u64)>,
}

impl Follower {
    /// Take the start position (D11): the stamp, then `last_seq`. The server awaits this before
    /// it accepts connections, so every change after the start is followed. When it fails, the
    /// loop tries again at its next step.
    pub async fn start(state: Arc<AppState>) -> Follower {
        let mut f = Follower {
            state,
            cursor: None,
            seen: None,
        };
        f.step().await;
        f
    }

    /// One follow step: take the start position, or read the new events when the journal
    /// changed. The stamp is read first: an append during the read changes it again.
    async fn step(&mut self) {
        let (at, seen) = (self.cursor, self.seen);
        let step = blocking(&self.state, move |st| -> crate::Result<_> {
            let stamp = journal_stamp(st)?;
            let next = match at {
                None => start_seq(st)?,
                Some(seq) if Some(stamp) == seen => seq,
                Some(mut seq) => {
                    follow_once(st, &mut seq)?;
                    seq
                }
            };
            Ok((stamp, next))
        })
        .await;
        match step {
            // Kept only after a good step, so a failed read is tried again.
            Some(Ok((stamp, next))) => {
                self.seen = Some(stamp);
                self.cursor = Some(next);
            }
            Some(Err(e)) => eprintln!("notedown cloud: push: journal: {e}"),
            None => {}
        }
    }

    /// The loop: follow every 1 s (only when the journal changed, D11) and tick every 500 ms.
    /// It never returns; the server aborts it when it stops.
    pub async fn run(mut self) {
        let mut follow = tokio::time::interval(FOLLOW_INTERVAL);
        let mut ticks = tokio::time::interval(TICK_INTERVAL);
        follow.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = follow.tick() => self.step().await,
                _ = ticks.tick() => {
                    blocking(&self.state, |st| tick(st, st.now_ms())).await;
                }
            }
        }
    }
}

/// The loop: follow every 1 s (only when the journal changed, D11) and tick every 500 ms. The
/// start position is taken first ([`Follower::start`]).
pub async fn run(state: Arc<AppState>) {
    Follower::start(state).await.run().await
}
