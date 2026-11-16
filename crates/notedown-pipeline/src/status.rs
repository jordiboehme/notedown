//! The pipeline status of one account (`notedown pipeline status` and the MCP `status` tool):
//! counts from the text store, what the worker wrote at the end of its last pass, the page in
//! work and the consumers. Read from the files only; nothing is created or written.

use std::path::Path;

use serde::Serialize;
use time::UtcOffset;

use crate::delta::{self, Start};
use crate::errors::Errors;
use crate::model::PageOcrState;
use crate::store::{LastError, TextStore, Work, WriterLock, source_error};

/// The status of one account. The serialized fields are the JSON of
/// `notedown pipeline status`; the skipped ones are for the MCP `status` tool. `waiting` and
/// `nextDueMs` are as of the worker's last pass; a stopped worker leaves them as they were.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusFacts {
    pub account: String,
    /// Whether a pipeline holds the writer lock of the data folder now.
    pub running: bool,
    /// Notes that are not deleted.
    pub notes: usize,
    pub pages_with_text: usize,
    /// Pages of notes the worker started on and has not read yet.
    pub pending: usize,
    /// Due pages of notes still within their settle time, as of the last pass.
    pub waiting: u64,
    /// When the next settle time or retry ends (ms): the last pass's value or the next retry
    /// of a failed page, whichever comes first; `None` when that time has passed.
    pub next_due_ms: Option<u64>,
    /// Pages whose tries are used up.
    pub failed: usize,
    /// Pages whose text was cut at the token limit.
    pub truncated: usize,
    /// Failed pages (and notes that do not parse) that are tried again.
    pub errors: usize,
    /// The last failure of the worker as a whole, of this account or of the source (the newer
    /// one); cleared by a good pass or a good look at the source.
    pub last_error: Option<LastError>,
    /// When the last pass ended (ms).
    pub last_run_ms: Option<u64>,
    /// The page the worker reads now; only while it runs.
    pub in_work: Option<Work>,
    pub consumers: Vec<ConsumerFacts>,
    /// Notes out of the scope (MCP only).
    #[serde(skip)]
    pub unscoped: usize,
    /// The `regionMissed` count of the stats (MCP only).
    #[serde(skip)]
    pub region_missed: u64,
}

/// One consumer of `get_changes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsumerFacts {
    pub name: String,
    /// The bookmark's `seq`.
    pub seq: u64,
    /// When it last acked (ms); `None` before its first ack.
    pub acked_at: Option<u64>,
    /// When it last called `get_changes` over MCP (ms).
    pub read_at: Option<u64>,
    /// The notes changed after its bookmark, up to the size of one manifest.
    pub notes_behind: usize,
    /// More changes follow after those notes.
    pub more: bool,
}

/// The status of account `slug` in the local folder `data` at `now_ms`. Creates no folder or
/// file: a missing store gives zero counts. The notes behind come from a manifest per
/// consumer, which reads the whole log, so a caller should not ask more often than every few
/// seconds.
pub fn status_facts(data: &Path, slug: &str, now_ms: u64) -> crate::Result<StatusFacts> {
    let store = TextStore::at(data, slug)?;
    let running = WriterLock::held(data)?;
    let notes: Vec<_> = store.notes()?.into_iter().filter(|n| !n.deleted).collect();
    let pages = || notes.iter().flat_map(|n| &n.pages);
    let count = |s: PageOcrState| pages().filter(|p| p.state == s).count();
    // A broken stats or error file is no reason to fail the status: it reads as none.
    let stats = store.stats().unwrap_or_default();
    let errors = Errors::load_quietly(store.dir())?;
    let next_due_ms = [stats.next_due_ms, errors.next_try(now_ms)]
        .into_iter()
        .flatten()
        .filter(|&t| t > now_ms)
        .min();
    let last_error = match (stats.last_error, source_error(data).ok().flatten()) {
        (Some(a), Some(b)) => Some(if b.at_ms > a.at_ms { b } else { a }),
        (a, b) => a.or(b),
    };
    // A file a killed worker left is not shown; a broken one is no reason to fail the status.
    let in_work = if running {
        store.work().ok().flatten()
    } else {
        None
    };
    let mut consumers = Vec::new();
    for (name, b) in store.consumers()? {
        let manifest = delta::manifest(&store, &Start::Bookmark(name.clone()), UtcOffset::UTC)?;
        consumers.push(ConsumerFacts {
            name,
            seq: b.seq,
            acked_at: (b.updated_at > 0).then_some(b.updated_at),
            read_at: b.read_at,
            notes_behind: manifest.notes.len(),
            more: manifest.more,
        });
    }
    Ok(StatusFacts {
        account: slug.to_string(),
        running,
        notes: notes.len(),
        pages_with_text: pages().filter(|p| !p.blocks.is_empty()).count(),
        pending: count(PageOcrState::Pending),
        waiting: stats.waiting,
        next_due_ms,
        failed: count(PageOcrState::Failed),
        truncated: count(PageOcrState::Truncated),
        errors: errors.entries().iter().filter(|e| !e.exhausted()).count(),
        last_error,
        last_run_ms: (stats.last_run_ms > 0).then_some(stats.last_run_ms),
        in_work,
        consumers,
        unscoped: notes.iter().filter(|n| n.unscoped).count(),
        region_missed: stats.region_missed,
    })
}
