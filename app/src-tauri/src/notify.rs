//! The desktop notifications of the Mac app: [`store`] keeps what they remember between two
//! starts, [`decide`] turns a status and that record into the notices to show.
//!
//! [`decide`] is pure: no file, no clock, no lock. It changes only the notifier's fields of a
//! [`KeyRecord`] (the poller writes the others). The record always moves, also with the
//! notifications off, so turning them on never shows old events. Missing data (no pipeline
//! facts yet, no account answer yet) never moves or clears a field, and a field seen for the
//! first time is a baseline without a notice.

pub mod store;

use std::path::{Path, PathBuf};

use crate::config::{Mode, NotificationChoices};
use crate::status::{AgentStatus, Attention, Connection, Status};
use crate::supervisor::ChildState;
use crate::windows::LibraryAt;
use store::{ConsumerRecord, KeyRecord, ProblemState};

/// At most one Sync and one OCR notice per this time (10 minutes); counts within it add up.
pub const GAP_MS: u64 = 10 * 60_000;
/// No notice for an event older than this (30 minutes): the app was not running.
pub const STALE_MS: u64 = 30 * 60_000;
/// At most one notice per problem per this time (30 minutes).
pub const PROBLEM_GAP_MS: u64 = 30 * 60_000;
/// An agent whose seq rose and did not rise again for this time (one pipeline poll while no
/// window is visible) read what it wanted.
pub const AGENT_QUIET_MS: u64 = 60_000;

/// The problem keys, as in `notedown.problem.<key>`.
pub const PROBLEM_SERVER: &str = "server";
pub const PROBLEM_OCR: &str = "ocr";
pub const PROBLEM_OLD_SYNC: &str = "oldsync";

/// The texts of the notices.
pub const SYNC_UNKNOWN: &str = "The Supernote synced.";
pub const SERVER_TEXT: &str = "The Notedown server does not answer.";
pub const OCR_ERROR_TEXT: &str = "The OCR stopped with an error.";
pub const OLD_SYNC_TEXT: &str = "The Supernote has not synced for 7 days.";

/// What [`decide`] looks at besides the record.
#[derive(Debug, Clone)]
pub struct NotifyInput {
    pub status: Status,
    /// `Notes::notes_newer_than(record.ocr_text_seq.unwrap_or(0))`, made only when
    /// [`wants_ocr_count`] says so; `None` otherwise.
    pub ocr_new: Option<(u64, Option<u64>)>,
    /// An import job holds the server (`ImportJobs::holds_server`).
    pub import_holds_server: bool,
    /// The token key of the login; `None` without a login, and nothing happens.
    pub key: Option<String>,
    /// The data folder of the config.
    pub data_dir: PathBuf,
    /// While the import mark of this login exists (`SetupRecord::sync_mark` with this key):
    /// its `sync_at_mark`, 0 when no device had synced. The first sync after it is shown on
    /// the Setup page, not in a notice.
    pub import_mark: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Sync,
    Ocr,
    Agent,
    /// One of [`PROBLEM_SERVER`], [`PROBLEM_OCR`], [`PROBLEM_OLD_SYNC`].
    Problem(&'static str),
}

impl NoticeKind {
    /// The request id: a new notice replaces the shown one of its kind.
    pub fn id(self) -> String {
        match self {
            Self::Sync => "notedown.sync".to_owned(),
            Self::Ocr => "notedown.ocr".to_owned(),
            Self::Agent => "notedown.agent".to_owned(),
            Self::Problem(key) => format!("notedown.problem.{key}"),
        }
    }

    /// The title; macOS shows the app name above it.
    pub fn title(self) -> &'static str {
        match self {
            Self::Sync => "Supernote",
            Self::Ocr => "OCR",
            Self::Agent => "Agent",
            Self::Problem(_) => "Needs attention",
        }
    }
}

/// Where a click on a notice goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Library(LibraryAt),
    /// A settings page by its id.
    Settings(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub kind: NoticeKind,
    pub title: String,
    pub body: String,
    pub target: Target,
}

impl Notice {
    fn new(kind: NoticeKind, body: String, target: Target) -> Self {
        Self {
            kind,
            title: kind.title().to_owned(),
            body,
            target,
        }
    }
}

/// The OCR is done: the status has facts, nothing waits or is in work, no error, and a pass
/// ran.
fn ocr_done(status: &Status) -> bool {
    let o = &status.ocr;
    o.facts
        && o.queued + o.pending + o.waiting == 0
        && o.in_work.is_none()
        && o.last_error.is_none()
        && o.last_run_ms.is_some()
}

/// Whether [`decide`] needs `ocr_new` for this status: the OCR is done and a pass ended since
/// the last count, or the marker needs its baseline.
pub fn wants_ocr_count(record: &KeyRecord, status: &Status, data_dir: &Path) -> bool {
    ocr_done(status)
        && (record.ocr_text_seq.is_none()
            || record.ocr_data_dir.as_deref() != Some(data_dir)
            || status.ocr.last_run_ms > record.ocr_last_run_ms)
}

/// When [`decide`] should run again with the last status although no new status came: an
/// agent whose seq rose waits for a quiet poll.
pub fn next_wake(record: &KeyRecord) -> Option<u64> {
    record
        .consumers
        .values()
        .filter(|c| c.pending_seq.is_some())
        .filter_map(|c| c.rose_at)
        .map(|at| at.saturating_add(AGENT_QUIET_MS))
        .min()
}

/// The notices for `input` and the record after it. Only the notifier's fields change.
pub fn decide(
    record: &KeyRecord,
    input: &NotifyInput,
    choices: &NotificationChoices,
    now_ms: u64,
) -> (Vec<Notice>, KeyRecord) {
    let mut next = record.clone();
    if input.key.is_none() {
        return (Vec::new(), next);
    }
    let set_up = input.status.setup.mode != Mode::NotSet;
    let on = |kind: bool| choices.enabled && kind && set_up;
    let mut notices = Vec::new();
    notices.extend(sync(&mut next, input, on(choices.sync), now_ms));
    notices.extend(ocr(&mut next, input, on(choices.ocr), now_ms));
    notices.extend(agents(&mut next, input, on(choices.agent), now_ms));
    let problems_on = on(choices.problems) && !input.import_holds_server;
    notices.extend(problems(&mut next, &input.status, problems_on, now_ms));
    (notices, next)
}

fn stale(at_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(at_ms) > STALE_MS
}

fn in_gap(last_notice_ms: Option<u64>, now_ms: u64) -> bool {
    last_notice_ms.is_some_and(|at| now_ms < at.saturating_add(GAP_MS))
}

fn sync(next: &mut KeyRecord, input: &NotifyInput, on: bool, now_ms: u64) -> Option<Notice> {
    // First sight: the baseline is what the poller has.
    let Some(shown) = next.sync_shown_at else {
        let at = next.last_count.map(|c| c.at_ms).or(next.last_sync_ms);
        next.sync_shown_at = Some(at.unwrap_or(0));
        return None;
    };
    let count = next.last_count.filter(|c| c.at_ms > shown)?;
    next.sync_shown_at = Some(count.at_ms);
    // The first sync after the import mark: the Setup page shows it.
    if input.import_mark.is_some_and(|mark| shown <= mark) {
        return None;
    }
    if count.files == Some(0) {
        return None;
    }
    if !on {
        next.unshown_files = 0;
        return None;
    }
    if stale(count.at_ms, now_ms) {
        return None;
    }
    if in_gap(next.sync_last_notice_ms, now_ms) {
        next.unshown_files = next.unshown_files.saturating_add(count.files.unwrap_or(0));
        return None;
    }
    let files = count.files.map(|f| f.saturating_add(next.unshown_files));
    next.unshown_files = 0;
    next.sync_last_notice_ms = Some(now_ms);
    let body = match files {
        Some(1) => "1 file changed.".to_owned(),
        Some(n) => format!("{n} files changed."),
        None => SYNC_UNKNOWN.to_owned(),
    };
    Some(Notice::new(
        NoticeKind::Sync,
        body,
        Target::Library(LibraryAt::All),
    ))
}

fn ocr(next: &mut KeyRecord, input: &NotifyInput, on: bool, now_ms: u64) -> Option<Notice> {
    let status = &input.status;
    let o = &status.ocr;
    if !o.facts {
        return None;
    }
    // Another data folder has other notes: a baseline.
    if next.ocr_data_dir.as_deref() != Some(input.data_dir.as_path()) {
        next.ocr_data_dir = Some(input.data_dir.clone());
        next.ocr_text_seq = None;
        next.ocr_last_run_ms = o.last_run_ms;
        next.unshown_notes = 0;
    }
    if next.ocr_last_run_ms.is_none() {
        next.ocr_last_run_ms = o.last_run_ms;
    }
    // Not done: the pass that ended counts with the one that leaves nothing to do.
    if !ocr_done(status) {
        return None;
    }
    let (count, newest) = input.ocr_new?;
    if next.ocr_text_seq.is_none() {
        next.ocr_text_seq = newest;
        next.ocr_last_run_ms = o.last_run_ms;
        return None;
    }
    if o.last_run_ms <= next.ocr_last_run_ms {
        return None;
    }
    next.ocr_last_run_ms = o.last_run_ms;
    next.ocr_text_seq = next.ocr_text_seq.max(newest);
    if count == 0 {
        return None;
    }
    if !on {
        next.unshown_notes = 0;
        return None;
    }
    if o.last_run_ms.is_some_and(|at| stale(at, now_ms)) {
        return None;
    }
    if in_gap(next.ocr_last_notice_ms, now_ms) {
        next.unshown_notes = next.unshown_notes.saturating_add(count);
        return None;
    }
    let notes = count.saturating_add(next.unshown_notes);
    next.unshown_notes = 0;
    next.ocr_last_notice_ms = Some(now_ms);
    let body = if notes == 1 {
        "1 note is ready.".to_owned()
    } else {
        format!("{notes} notes are ready.")
    };
    Some(Notice::new(
        NoticeKind::Ocr,
        body,
        Target::Library(LibraryAt::New),
    ))
}

fn agents(next: &mut KeyRecord, input: &NotifyInput, on: bool, now_ms: u64) -> Vec<Notice> {
    let status = &input.status;
    // Without facts the list says nothing: the consumers stay as they are.
    if !status.ocr.facts {
        return Vec::new();
    }
    let mut notices = Vec::new();
    let mut kept = std::collections::BTreeMap::new();
    for agent in &status.agents {
        let record = match next.consumers.get(&agent.name) {
            // A newly seen consumer, or one that starts over: a baseline.
            Some(r) if agent.seq >= r.seq.max(r.pending_seq.unwrap_or(0)) => {
                let (record, read) = consumer(r.clone(), agent, now_ms);
                if read && on && !agent.acked_at_ms.is_some_and(|at| stale(at, now_ms)) {
                    notices.push(Notice::new(
                        NoticeKind::Agent,
                        format!("{} read the new notes.", agent.name),
                        Target::Settings("agents".to_owned()),
                    ));
                }
                record
            }
            _ => ConsumerRecord {
                seq: agent.seq,
                pending_seq: None,
                rose_at: None,
            },
        };
        kept.insert(agent.name.clone(), record);
    }
    next.consumers = kept;
    notices
}

/// One consumer after `agent`: true when it read what it wanted (its seq rose and it is up to
/// date, or did not rise again for [`AGENT_QUIET_MS`]).
fn consumer(
    mut record: ConsumerRecord,
    agent: &AgentStatus,
    now_ms: u64,
) -> (ConsumerRecord, bool) {
    let high = record.pending_seq.unwrap_or(record.seq);
    if agent.seq > high {
        record.pending_seq = Some(agent.seq);
        record.rose_at = Some(now_ms);
    }
    let Some(pending) = record.pending_seq else {
        return (record, false);
    };
    let quiet = record
        .rose_at
        .is_some_and(|at| now_ms >= at.saturating_add(AGENT_QUIET_MS));
    if agent.notes_behind == 0 || quiet {
        record.seq = pending;
        record.pending_seq = None;
        record.rose_at = None;
        return (record, true);
    }
    (record, false)
}

/// What the status says of one problem: there, gone, or not known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    There,
    Gone,
    Unknown,
}

fn problems(next: &mut KeyRecord, status: &Status, on: bool, now_ms: u64) -> Vec<Notice> {
    let has = |a: Attention| status.attention.contains(&a);
    let seen = |there: bool, gone: bool| {
        if there {
            Seen::There
        } else if gone {
            Seen::Gone
        } else {
            Seen::Unknown
        }
    };
    let server = seen(
        has(Attention::ServerUnreachable),
        status.connection == Connection::Reached,
    );
    let ocr = seen(
        has(Attention::OcrError) || has(Attention::PipelineBackoff),
        status.ocr.facts
            && status.ocr.last_error.is_none()
            && !matches!(status.pipeline.state, ChildState::Backoff { .. }),
    );
    let old_sync = seen(
        has(Attention::OldSync),
        status.device.last_sync_ms.is_some(),
    );
    let sync_at = status.device.last_sync_ms;
    let mut notices = Vec::new();
    let list = [
        (PROBLEM_SERVER, server, None, SERVER_TEXT, "server"),
        (PROBLEM_OCR, ocr, None, OCR_ERROR_TEXT, "ocr"),
        (
            PROBLEM_OLD_SYNC,
            old_sync,
            sync_at,
            OLD_SYNC_TEXT,
            "supernote",
        ),
    ];
    for (key, seen, for_sync, text, page) in list {
        if problem(next, key, seen, for_sync, on, now_ms) {
            notices.push(Notice::new(
                NoticeKind::Problem(key),
                text.to_owned(),
                Target::Settings(page.to_owned()),
            ));
        }
    }
    notices
}

/// Moves the state of the problem `key`; true when it gets a notice. A problem is new when it
/// was not there before (the old sync problem: for another `lastSyncMs`).
fn problem(
    next: &mut KeyRecord,
    key: &str,
    seen: Seen,
    for_sync: Option<u64>,
    on: bool,
    now_ms: u64,
) -> bool {
    match (seen, next.problems.get_mut(key)) {
        (Seen::Unknown, _) => false,
        (Seen::Gone, Some(state)) => {
            state.active = false;
            false
        }
        (Seen::Gone, None) => {
            next.problems
                .insert(key.to_owned(), ProblemState::default());
            false
        }
        // First sight: a baseline.
        (Seen::There, None) => {
            next.problems.insert(
                key.to_owned(),
                ProblemState {
                    active: true,
                    last_notice_ms: 0,
                    for_sync,
                },
            );
            false
        }
        (Seen::There, Some(state)) => {
            let new = if key == PROBLEM_OLD_SYNC {
                state.for_sync != for_sync
            } else {
                !state.active
            };
            state.active = true;
            state.for_sync = for_sync;
            let due = state.last_notice_ms == 0
                || now_ms >= state.last_notice_ms.saturating_add(PROBLEM_GAP_MS);
            if new && on && due {
                state.last_notice_ms = now_ms;
                return true;
            }
            false
        }
    }
}

#[cfg(test)]
mod tests;
