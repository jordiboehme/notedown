//! The journal follower and the push loop.
//!
//! The follower reads the journal events after its cursor and feeds them to the planner. The
//! cursor lives in memory only: it starts at the journal's `last_seq` when the server starts
//! (D11), so older changes never cause a push, and a restart drops pending pushes (D6). The tick
//! hands every due push that D8 allows to the device's socket session: `detail` and `startsync`
//! put their items into its queue, which the session sends after its next heartbeat reply
//! (`docs/sync-protocol.md` 4.4).

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

/// One line on stderr and one marker in the protocol log.
fn log_marker(state: &AppState, message: String) {
    eprintln!("notedown cloud: {message}");
    let mut m = LogEntry::new(LogKind::Marker);
    m.message = Some(message);
    state.log.record(m);
}

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
        // A session whose last push still waits for `Received` gets no new items: the changes
        // go on collapsing in the planner, so the session queue stays small.
        if !open
            || state.sync_locks.holder(&slug, now_ms).is_some()
            || !state.sockets.is_connected(&slug, &serial, now_ms)
            || state.sockets.awaiting_received(&slug, &serial)
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
        let items = if state.config.push == PushMode::Startsync {
            vec![startsync_item(pending.cause.as_deref(), now_ms)]
        } else {
            // Every pending change, however many (Ratta has no cap either, 4.4). When the id
            // registry can't be read, the push stays pending and is tried again after a
            // heartbeat; only the first failure in a row is logged.
            let built = state
                .store(&slug)
                .and_then(|store| detail_items(&pending.changes, &store, now_ms));
            match built {
                Ok(items) => items,
                Err(e) => {
                    if planner.failed(&slug, &serial, now_ms) {
                        log_marker(
                            state,
                            format!(
                                "push: the changes for {serial} of account {slug} can't be \
                                 sent, the push waits and is tried again: {e}"
                            ),
                        );
                    }
                    continue;
                }
            }
        };
        // An entry created and deleted again leaves nothing to tell: the push is done without a
        // message.
        let recovered = if items.is_empty() {
            planner.drop_pending(&slug, &serial)
        } else if state.sockets.queue(&slug, &serial, items, now_ms) {
            sent += 1;
            planner.sent(&slug, &serial, now_ms)
        } else {
            false
        };
        if recovered {
            log_marker(
                state,
                format!("push: the changes for {serial} of account {slug} can be sent again"),
            );
        }
    }
    sent
}

/// The push gate (D8): with `protect = true` an account gets pushes only after its import, and
/// no account gets pushes while a scan holds (`scan-hold.json` exists or can't be read): the
/// device would only meet `E0079`.
fn gate_open(state: &AppState, slug: &str) -> bool {
    let hold = state
        .auth
        .accounts()
        .account_dir(slug)
        .join(crate::store::SCAN_HOLD_FILE);
    match std::fs::symlink_metadata(&hold) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => return false,
        Err(e) => {
            eprintln!(
                "notedown cloud: push: account {slug}: {}: {e}",
                hold.display()
            );
            return false;
        }
    }
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
