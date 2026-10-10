//! The OCR worker (spec 4): finds what is due, waits for the settle time, reads page by page
//! and keeps the text store.
//!
//! - [`due`] (in `worker/due.rs`) compares the page records of an account with its note states
//!   and writes the lines of whole notes. Journal events only mean "look now": a missed event
//!   leaves no gap, because the records are compared every time.
//! - `worker/note.rs` reads one due note and writes each page at once.
//! - This file holds the queue (order, settle time), the model (loaded when a page is due,
//!   unloaded after `model_idle`) and the loop over the journal.
//!
//! Settle time: the time of a note's last page change is the mtime of its page record. The
//! server writes the record only when the note's bytes change (or the note moves), right
//! before the journal append of that change, so it is there for every note, also without
//! journal events.

mod due;
mod note;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notedown_cloud::appdata::Digest;
use notedown_cloud::auth::Accounts;
use notedown_cloud::journal::Journal;
use notedown_ocr::{OcrEngine, OcrError};

pub use due::{AccountStore, DueNote, due};

use crate::errors::Errors;
use crate::records;
use crate::retention;
use crate::scope::in_scope;
use crate::store::{TextStore, WriterLock};
use crate::{Error, PipelineConfig, Result};
use note::{Mode, NoteJob};

/// The journal consumer name of the worker: `journal/consumers/pipeline.cursor`.
pub const CONSUMER: &str = "pipeline";
/// How often a missing model is looked for again.
pub const MODEL_RETRY: Duration = Duration::from_secs(10 * 60);
/// How often the continuous worker looks at the journal.
const POLL: Duration = Duration::from_secs(1);
/// A full look at the records at least this often, also without journal events.
const RESCAN_MS: u64 = 10 * 60 * 1000;
/// After a failed pass, the next one.
const ERROR_WAIT_MS: u64 = 60 * 1000;
/// A failing prune is said at most this often per account.
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// The time source of the worker. Tests drive it; [`SystemClock`] is the real one.
pub trait Clock: Send + Sync {
    /// Ms since the epoch.
    fn now_ms(&self) -> u64;
    fn sleep(&self, d: Duration);
}

/// The system time and a real sleep.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64)
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// Loads the OCR model. `OcrError::ModelMissing` makes the worker wait for it.
pub type EngineLoader =
    Box<dyn FnMut() -> std::result::Result<Box<dyn OcrEngine>, OcrError> + Send>;

enum Engine {
    /// Given loaded; never unloaded.
    Fixed(Box<dyn OcrEngine>),
    /// Loaded when a page is due, unloaded after `model_idle`.
    Lazy {
        loader: EngineLoader,
        loaded: Option<Box<dyn OcrEngine>>,
    },
}

/// The engine with its load state.
struct EngineSlot {
    engine: Engine,
    /// No load try before this (ms), after a failed one.
    retry_at: u64,
    /// The last page read (ms).
    last_used: u64,
}

impl EngineSlot {
    /// The engine, loaded if needed. A model that cannot be loaded prints one line and is
    /// tried again after [`MODEL_RETRY`].
    fn get(&mut self, now: u64) -> Result<&mut dyn OcrEngine> {
        match &mut self.engine {
            Engine::Fixed(e) => Ok(e.as_mut()),
            Engine::Lazy { loader, loaded } => {
                if loaded.is_none() {
                    if now < self.retry_at {
                        return Err(Error::ModelMissing);
                    }
                    match loader() {
                        Ok(e) => *loaded = Some(e),
                        Err(e) => {
                            // Tries are MODEL_RETRY apart, so this is one line per 10 minutes.
                            self.retry_at = now + MODEL_RETRY.as_millis() as u64;
                            match e {
                                OcrError::ModelMissing { .. } => {
                                    eprintln!("{}", Error::ModelMissing)
                                }
                                e => eprintln!("the OCR model cannot be loaded: {e}"),
                            }
                            return Err(Error::ModelMissing);
                        }
                    }
                }
                Ok(loaded.as_mut().expect("loaded above").as_mut())
            }
        }
    }

    fn used(&mut self, now: u64) {
        self.last_used = now;
    }

    fn loaded(&self) -> bool {
        match &self.engine {
            Engine::Fixed(_) => true,
            Engine::Lazy { loaded, .. } => loaded.is_some(),
        }
    }

    fn unload_if_idle(&mut self, now: u64, idle: Duration) {
        if let Engine::Lazy { loaded, .. } = &mut self.engine
            && loaded.is_some()
            && now.saturating_sub(self.last_used) >= idle.as_millis() as u64
        {
            *loaded = None;
        }
    }
}

/// What a pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    /// Pages handed to the engine.
    pub attempted: usize,
    /// Pages read.
    pub pages_read: usize,
    /// Page text versions written.
    pub versions: usize,
    /// Failed reads (pages, and notes that do not parse).
    pub failed: usize,
    /// Due pages of notes still within their settle time.
    pub waiting: usize,
    /// Untouched blocks the safety net gave a new reading.
    pub region_missed: u64,
    /// When the next settle time or retry ends (ms), if any.
    pub next_due: Option<u64>,
}

/// The OCR worker of one data folder. Every call that writes takes the writer lock
/// (`<data>/pipeline/.writer.lock`) for its duration.
pub struct Worker {
    data: PathBuf,
    cfg: PipelineConfig,
    engine: EngineSlot,
    clock: Arc<dyn Clock>,
    /// The pages already reported as missing from their note file, so each is told once
    /// until its record changes.
    told_missing: RefCell<BTreeSet<String>>,
}

impl Worker {
    /// A worker with a loaded `engine`.
    pub fn new(data: &Path, cfg: PipelineConfig, engine: Box<dyn OcrEngine>) -> Worker {
        Worker::with_engine(data, cfg, Engine::Fixed(engine))
    }

    /// A worker that loads the model with `loader` when a page is due and unloads it after
    /// `model_idle` without work.
    pub fn with_loader(data: &Path, cfg: PipelineConfig, loader: EngineLoader) -> Worker {
        Worker::with_engine(
            data,
            cfg,
            Engine::Lazy {
                loader,
                loaded: None,
            },
        )
    }

    fn with_engine(data: &Path, cfg: PipelineConfig, engine: Engine) -> Worker {
        Worker {
            data: data.to_path_buf(),
            cfg,
            engine: EngineSlot {
                engine,
                retry_at: 0,
                last_used: 0,
            },
            clock: Arc::new(SystemClock),
            told_missing: RefCell::default(),
        }
    }

    /// Uses `clock` instead of the system clock.
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Worker {
        self.clock = clock;
        self
    }

    /// Whether the model is loaded now.
    pub fn model_loaded(&self) -> bool {
        self.engine.loaded()
    }

    /// One pass over all accounts: the due notes whose settle time is over (all of them with
    /// `ignore_settle`), page by page, each page written at once.
    pub fn run_once(&mut self, ignore_settle: bool) -> Result<Progress> {
        let lock = WriterLock::take(&self.data)?;
        let mut stores = BTreeMap::new();
        self.pass(&lock, &mut stores, ignore_settle, &|| false)
    }

    /// Reads note `note` (a key or a wire path) or, with `None`, every note in scope again,
    /// with the whole page as the change region. Changed text gives normal versions.
    pub fn reocr(&mut self, note: Option<&str>) -> Result<Progress> {
        let lock = WriterLock::take(&self.data)?;
        let mut progress = Progress::default();
        let mut found = false;
        for slug in self.slugs()? {
            let text = TextStore::open_writer(&self.data, &slug, &lock)?;
            let store = AccountStore {
                text: &text,
                lock: &lock,
            };
            let now = self.clock.now_ms();
            // Note changes (moves, scope) first, so the states are current.
            due(&self.data, &slug, &store, &self.cfg, now)?;
            let recs = records::read(&self.data, &slug)?;
            let digests = records::digests(&self.data, &slug)?;
            for (key, rec) in &recs.records {
                let r = &rec.record;
                if note.is_some_and(|n| n != key && n != r.path) {
                    continue;
                }
                let state = text.note(key)?;
                let words = state
                    .as_ref()
                    .map(|s| s.meta.keywords.clone())
                    .unwrap_or_default();
                if !in_scope(&self.cfg, &r.path, &words) {
                    continue;
                }
                found = true;
                let d = DueNote {
                    key: key.clone(),
                    path: r.path.clone(),
                    since: rec.changed_ms,
                    pages: r.pages.iter().map(|p| p.page_id.clone()).collect(),
                    new: false,
                };
                let clock = self.clock.clone();
                let job = NoteJob {
                    data: &self.data,
                    slug: &slug,
                    store,
                    records: &recs.records,
                    digests: &digests,
                    now: &|| clock.now_ms(),
                    mode: Mode::Reocr,
                    told_missing: &self.told_missing,
                };
                note::process(
                    &job,
                    &mut self.engine,
                    &d,
                    &mut progress,
                    &mut |_| {},
                    &|| false,
                )?;
            }
            text.set_last_run(&lock, self.clock.now_ms())?;
        }
        if let Some(n) = note
            && !found
        {
            return Err(Error::Invalid {
                what: "note",
                message: format!("{n:?} is not a note in scope"),
            });
        }
        Ok(progress)
    }

    /// The continuous worker: tails the journal with the cursor `pipeline`, looks at the
    /// records at start, after each journal batch, when a settle time or retry ends and every
    /// 10 minutes, and reads what is due. The model is unloaded after `model_idle` without
    /// work. After each pass it prunes the text log of every account (at most once a day,
    /// spec 2.4). Returns when `stop` says so; it is asked between pages too.
    pub fn run(&mut self, stop: impl Fn() -> bool) -> Result<()> {
        self.run_with_start(|| {}, stop)
    }

    /// [`run`](Self::run), calling `started` once the writer lock is held (a refused second
    /// writer never calls it).
    pub fn run_with_start(
        &mut self,
        started: impl FnOnce(),
        stop: impl Fn() -> bool,
    ) -> Result<()> {
        let lock = WriterLock::take(&self.data)?;
        started();
        // A failing prune is said at most once a day per account.
        let mut prune_warned: BTreeMap<String, u64> = BTreeMap::new();
        let journal = Journal::open_no_create(&self.data);
        let mut done = journal.cursor(CONSUMER)?.map_or(0, |c| c.seq);
        let mut tail = journal.tail(done + 1);
        let mut stores = BTreeMap::new();
        let mut look = true;
        let mut next_wake = 0u64;
        let mut warned: Option<String> = None;
        loop {
            if journal.dir().is_dir() {
                match tail.next_batch() {
                    Ok(batch) => {
                        look |= !batch.is_empty();
                        warned = None;
                    }
                    Err(e) => {
                        let message = e.to_string();
                        if warned.as_ref() != Some(&message) {
                            eprintln!("notedown pipeline: journal: {message}");
                            warned = Some(message);
                        }
                    }
                }
            }
            let now = self.clock.now_ms();
            if look || now >= next_wake {
                look = false;
                let seen = tail.next_seq().saturating_sub(1);
                let result = self.pass(&lock, &mut stores, false, &stop);
                let now = self.clock.now_ms();
                next_wake = now + RESCAN_MS;
                match result {
                    Ok(p) => {
                        // Old page versions go once a day (the prune's own stamp limits it).
                        for (slug, text) in stores.iter() {
                            if let Err(e) =
                                retention::prune(text, &lock, self.cfg.text_keep_days, now)
                                && prune_warned
                                    .get(slug)
                                    .is_none_or(|&at| now.saturating_sub(at) >= DAY_MS)
                            {
                                eprintln!("notedown pipeline: account {slug}: prune: {e}");
                                prune_warned.insert(slug.clone(), now);
                            }
                        }
                        if let Some(t) = p.next_due {
                            next_wake = next_wake.min(t.max(now));
                        }
                        if seen > done {
                            match journal.set_cursor(CONSUMER, seen) {
                                Ok(_) => done = seen,
                                Err(e) => eprintln!("notedown pipeline: journal cursor: {e}"),
                            }
                        }
                    }
                    Err(Error::ModelMissing) => {
                        next_wake = next_wake.min(self.engine.retry_at.max(now));
                    }
                    Err(e) => {
                        eprintln!("notedown pipeline: {e}");
                        next_wake = now + ERROR_WAIT_MS;
                    }
                }
            }
            self.engine
                .unload_if_idle(self.clock.now_ms(), self.cfg.model_idle);
            if stop() {
                return Ok(());
            }
            self.clock.sleep(POLL);
        }
    }

    fn slugs(&self) -> Result<Vec<String>> {
        Ok(Accounts::new(&self.data)
            .list()?
            .into_iter()
            .map(|a| a.slug)
            .collect())
    }

    /// One pass: the due notes of every account in one queue. Notes with text go first, the
    /// one whose settle time ended first at the front; then notes without any text yet (the
    /// first run), newest first, with progress on stderr. An account that fails is reported
    /// and skipped; a missing model ends the pass.
    fn pass(
        &mut self,
        lock: &WriterLock,
        stores: &mut BTreeMap<String, TextStore>,
        ignore_settle: bool,
        stop: &dyn Fn() -> bool,
    ) -> Result<Progress> {
        let now = self.clock.now_ms();
        let settle = self.cfg.ocr_settle.as_millis() as u64;
        let mut progress = Progress::default();
        let mut queue: Vec<(String, DueNote)> = Vec::new();
        let mut recs: BTreeMap<String, (records::Records, Vec<Digest>)> = BTreeMap::new();
        let mut next_due: Option<u64> = None;
        for slug in self.slugs()? {
            let found = (|| -> Result<Vec<DueNote>> {
                if !stores.contains_key(&slug) {
                    let text = TextStore::open_writer(&self.data, &slug, lock)?;
                    stores.insert(slug.clone(), text);
                }
                let text = &stores[&slug];
                let store = AccountStore { text, lock };
                let notes = due(&self.data, &slug, &store, &self.cfg, now)?;
                if let Some(t) = Errors::load(text.dir())?.next_try(now) {
                    next_due = Some(next_due.map_or(t, |n| n.min(t)));
                }
                recs.insert(
                    slug.clone(),
                    (
                        records::read(&self.data, &slug)?,
                        records::digests(&self.data, &slug)?,
                    ),
                );
                Ok(notes)
            })();
            let notes = match found {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("notedown pipeline: account {slug}: {e}");
                    // A failed write blocks the store until a repair: open it again next time.
                    stores.remove(&slug);
                    continue;
                }
            };
            for d in notes {
                let ends = d.since.saturating_add(settle);
                if ignore_settle || ends <= now {
                    queue.push((slug.clone(), d));
                } else {
                    progress.waiting += d.pages.len();
                    next_due = Some(next_due.map_or(ends, |n| n.min(ends)));
                }
            }
        }
        queue.sort_by(|(sa, a), (sb, b)| {
            a.new
                .cmp(&b.new)
                .then_with(|| {
                    if a.new {
                        b.since.cmp(&a.since)
                    } else {
                        a.since.cmp(&b.since)
                    }
                })
                .then_with(|| sa.cmp(sb))
                .then_with(|| a.key.cmp(&b.key))
        });
        let first_run = queue.iter().any(|(_, d)| d.new);
        let total: usize = queue.iter().map(|(_, d)| d.pages.len()).sum();
        let mut report = |p: &Progress| {
            if first_run {
                eprintln!("ocr: {} of {total} pages", p.attempted);
            }
        };
        let clock = self.clock.clone();
        let now_fn = || clock.now_ms();
        let mut broken: Vec<String> = Vec::new();
        for (slug, d) in &queue {
            if broken.contains(slug) {
                continue;
            }
            if stop() {
                break;
            }
            let (Some(text), Some((r, digests))) = (stores.get(slug), recs.get(slug)) else {
                continue;
            };
            let job = NoteJob {
                data: &self.data,
                slug,
                store: AccountStore { text, lock },
                records: &r.records,
                digests,
                now: &now_fn,
                mode: Mode::Due,
                told_missing: &self.told_missing,
            };
            match note::process(&job, &mut self.engine, d, &mut progress, &mut report, stop) {
                Ok(()) => {}
                Err(Error::ModelMissing) => return Err(Error::ModelMissing),
                Err(e) => {
                    eprintln!("notedown pipeline: {}: {e}", d.path);
                    broken.push(slug.clone());
                }
            }
        }
        // A failed write blocks the store until a repair: open it again next pass.
        for slug in broken {
            stores.remove(&slug);
        }
        let end = self.clock.now_ms();
        for text in stores.values() {
            text.set_last_run(lock, end)?;
        }
        progress.next_due = next_due;
        Ok(progress)
    }
}
