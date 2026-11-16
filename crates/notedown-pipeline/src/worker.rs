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
//! Two roots: the server's data is read only through [`ServerData`] (`setup.source`), and the
//! worker's own data (text stores, the writer lock, one journal cursor per account) is in the
//! local folder (`setup.local`). On one machine both are the same folder. Every error of the
//! source means "nothing known": the account is skipped and looked at again, its cursor stays,
//! and no note counts as deleted.
//!
//! Note text is deleted only for an account that is gone for sure: its `account.removed` event
//! names the user id of the store's owner (`owner.json`) and the server does not list the slug
//! with that id any more, or the server lists the slug with another user id (a new account).
//! Nothing else deletes a store: not a source error, not an empty or missing server folder,
//! not an account the server no longer lists. A removal that fails is tried again every loop
//! turn, and the account's cursor stays before it.
//!
//! Settle time: the time of a note's last page change is the mtime of its page record. The
//! server writes the record only when the note's bytes change (or the note moves), right
//! before the journal append of that change, so it is there for every note, also without
//! journal events.

mod due;
mod note;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use notedown_cloud::appdata::Digest;
use notedown_ocr::{OcrEngine, OcrError};

pub use due::{AccountStore, DueNote, KeywordCache, due};

use crate::cursor;
use crate::errors::Errors;
use crate::records;
use crate::retention;
use crate::scope::in_scope;
use crate::setup::Setup;
use crate::source::{Removal, ServerAccount, ServerData};
use crate::store::{LastError, TextStore, WriterLock};
use crate::{Error, Result};
use note::{Mode, NoteJob};

/// The consumer name of the one cursor that one machine kept before the per-account cursors
/// (`journal/consumers/pipeline.cursor`). It is only read, to take its place over.
pub const CONSUMER: &str = "pipeline";
/// The most journal lines read in one go; more are read before the worker sleeps.
const JOURNAL_PAGE: usize = 10_000;
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

/// Where the worker is in the journal of one account.
struct Follow {
    /// The last `seq` read, taken before each pass.
    read: u64,
    /// The cursor in the local folder; `None` when there is no cursor file yet.
    saved: Option<u64>,
}

/// The accounts of one loop turn.
struct Followed {
    /// Every account the server lists, unfiltered: tells a live account from a removed one.
    all: Vec<ServerAccount>,
    /// The accounts the worker reads ([`Setup::served`]): the owner check and the passes.
    served: Vec<ServerAccount>,
    /// The accounts whose journals the worker reads: the served ones and the local stores
    /// (on two machines only those with an owner), so a removal is seen after the server
    /// stopped listing the account.
    journals: Vec<String>,
}

/// The removals of each account not carried out yet, by `seq`.
type Pending = BTreeMap<String, BTreeMap<u64, Waiting>>;

/// A removal not carried out yet.
#[derive(Debug, Clone, Copy)]
struct Waiting {
    user_id: u64,
    /// Read in this turn, after its account list: a list that still shows the account alive
    /// may be from before the removal, so "alive" is only believed from a later list.
    fresh: bool,
}

/// What a removal came to.
enum Outcome {
    /// Done: the store is deleted, or there is none, or it is another account's.
    Done,
    /// The server lists the account as alive.
    Alive,
    /// The server leaves the account out but does not know it is gone (a refused token, a
    /// failed request): the removal stays and nothing is deleted.
    Wait,
}

/// Where the worker's own messages go.
pub type Messages = Arc<dyn Fn(&str) + Send + Sync>;

/// The OCR worker of one pipeline. Every call that writes takes the writer lock
/// (`<local>/pipeline/.writer.lock`) for its duration.
pub struct Worker {
    setup: Setup,
    engine: EngineSlot,
    clock: Arc<dyn Clock>,
    /// The pages already reported as missing from their note file, so each is told once
    /// until its record changes.
    told_missing: RefCell<BTreeSet<String>>,
    /// The keywords of notes out of scope, so the keyword scope parses each note once.
    keywords: RefCell<KeywordCache>,
    say: Messages,
    /// The last message said per topic, so a failure that repeats is said once.
    said: RefCell<BTreeMap<String, String>>,
}

impl Worker {
    /// A worker with a loaded `engine`.
    pub fn new(setup: &Setup, engine: Box<dyn OcrEngine>) -> Worker {
        Worker::with_engine(setup, Engine::Fixed(engine))
    }

    /// A worker that loads the model with `loader` when a page is due and unloads it after
    /// `model_idle` without work.
    pub fn with_loader(setup: &Setup, loader: EngineLoader) -> Worker {
        Worker::with_engine(
            setup,
            Engine::Lazy {
                loader,
                loaded: None,
            },
        )
    }

    fn with_engine(setup: &Setup, engine: Engine) -> Worker {
        Worker {
            setup: setup.clone(),
            engine: EngineSlot {
                engine,
                retry_at: 0,
                last_used: 0,
            },
            clock: Arc::new(SystemClock),
            told_missing: RefCell::default(),
            keywords: RefCell::default(),
            say: Arc::new(|m: &str| eprintln!("{m}")),
            said: RefCell::default(),
        }
    }

    /// Uses `clock` instead of the system clock.
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Worker {
        self.clock = clock;
        self
    }

    /// Sends the worker's own messages (accounts, journals, cursors, failed passes) to `say`
    /// instead of stderr. The messages of single pages stay on stderr.
    pub fn with_messages(mut self, say: Messages) -> Worker {
        self.say = say;
        self
    }

    /// Whether the model is loaded now.
    pub fn model_loaded(&self) -> bool {
        self.engine.loaded()
    }

    fn say(&self, message: &str) {
        (self.say)(message);
    }

    /// Says `message` unless it is the last one said on `topic`.
    fn say_once(&self, topic: &str, message: String) {
        let mut said = self.said.borrow_mut();
        if said.get(topic) != Some(&message) {
            (self.say)(&message);
            said.insert(topic.to_string(), message);
        }
    }

    /// `topic` is fine again: its next failure is said.
    fn settled(&self, topic: &str) {
        self.said.borrow_mut().remove(topic);
    }

    /// The accounts the worker follows. The one place this set is made. A local folder that
    /// cannot be listed is said and left out for this turn (its cursors stay).
    fn followed(&self) -> Result<Followed> {
        let all = self.setup.source.accounts();
        self.say_refusals();
        let all = all?;
        let served = self.setup.serve(&all);
        let mut journals: Vec<String> = served.iter().map(|a| a.slug.clone()).collect();
        match crate::setup::local_stores(&self.setup.local, self.setup.two_machines) {
            Ok(local) => {
                self.settled("stores");
                for slug in local {
                    if !journals.contains(&slug) {
                        journals.push(slug);
                    }
                }
            }
            Err(e) => self.say_once("stores", format!("notedown pipeline: {e}")),
        }
        Ok(Followed {
            all,
            served,
            journals,
        })
    }

    /// Says each refused token of the source once (topic `token:<slug>`); a refusal that is
    /// gone is settled, so it is said again when it comes back.
    fn say_refusals(&self) {
        let refusals = self.setup.source.refusals();
        let topics: Vec<String> = refusals
            .iter()
            .map(|(slug, _)| format!("token:{slug}"))
            .collect();
        let gone: Vec<String> = self
            .said
            .borrow()
            .keys()
            .filter(|t| t.starts_with("token:") && !topics.contains(t))
            .cloned()
            .collect();
        for topic in gone {
            self.settled(&topic);
        }
        for (topic, (_, message)) in topics.iter().zip(refusals) {
            self.say_once(topic, format!("notedown pipeline: {message}"));
        }
    }

    /// Takes the writer lock, sweeps the tombstones a crash or a failed delete left and
    /// removes the work files a killed worker left.
    fn take_lock(&self) -> Result<WriterLock> {
        let lock = WriterLock::take(&self.setup.local)?;
        self.sweep(&lock);
        self.clear_work(&lock);
        Ok(lock)
    }

    /// Removes `work.json` of every store in the local folder. Creates nothing; a file that
    /// cannot be removed only shows an old page while the worker runs.
    fn clear_work(&self, lock: &WriterLock) {
        let Ok(slugs) = crate::setup::local_stores(&self.setup.local, false) else {
            return;
        };
        for slug in slugs {
            if let Ok(text) = TextStore::at(&self.setup.local, &slug) {
                let _ = text.clear_work(lock);
            }
        }
    }

    /// Keeps `text` as the last error of account `slug` for the status, in a store that
    /// exists. Best effort: the error was said already.
    fn keep_error(&self, lock: &WriterLock, slug: &str, text: String) {
        if let Ok(store) = TextStore::at(&self.setup.local, slug) {
            let _ = store.set_last_error(
                lock,
                LastError {
                    text,
                    at_ms: self.clock.now_ms(),
                },
            );
        }
    }

    /// Deletes the tombstones under `<local>/pipeline`; the ones that stay are said once.
    fn sweep(&self, lock: &WriterLock) {
        let swept = TextStore::sweep_all(&self.setup.local, lock);
        if swept.failed.is_empty() {
            self.settled("sweep");
            return;
        }
        let list: Vec<String> = swept
            .failed
            .iter()
            .map(|(path, e)| format!("{}: {e}", path.display()))
            .collect();
        self.say_once(
            "sweep",
            format!(
                "notedown pipeline: deleted note text cannot be removed from the disk ({}); it is \
                 tried again at the next start or removal",
                list.join("; ")
            ),
        );
    }

    /// The owner check of a served account before its pass (`stores` holds its store). Gives
    /// whether the account may be read now.
    /// - No owner and no text: the store is new; the owner is its first write.
    /// - No owner but text: on one machine it is the account's own (creating and removing an
    ///   account there delete the text); on two machines nobody can tell, so the account is
    ///   skipped until the owner file is copied or the store is deleted.
    /// - Another owner: the slug belongs to a new account. The old text is deleted.
    fn owner_check(
        &self,
        lock: &WriterLock,
        stores: &mut BTreeMap<String, TextStore>,
        account: &ServerAccount,
    ) -> Result<bool> {
        let slug = &account.slug;
        let text = &stores[slug];
        let owner = text.owner()?;
        let topic = format!("owner:{slug}");
        match owner {
            Some(id) if id == account.user_id => {}
            Some(old) => {
                stores.remove(slug);
                TextStore::destroy(&self.setup.local, slug, lock)?;
                self.sweep(lock);
                self.say(&format!(
                    "notedown pipeline: account {slug} belongs to a new user id ({}, was \
                     {old}); its old note text is deleted",
                    account.user_id
                ));
                let text = TextStore::open_writer(&self.setup.local, slug, lock)?;
                text.set_owner(account.user_id, lock)?;
                stores.insert(slug.clone(), text);
            }
            None => {
                let empty = text.notes()?.is_empty() && text.last_seq()? == 0;
                if !empty && self.setup.two_machines {
                    self.say_once(
                        &topic,
                        format!(
                            "notedown pipeline: {} has note text but no owner; copy it from a \
                             pipeline that ran this version, or delete it to read everything \
                             again",
                            text.dir().display()
                        ),
                    );
                    return Ok(false);
                }
                text.set_owner(account.user_id, lock)?;
            }
        }
        self.settled(&topic);
        Ok(true)
    }

    /// Carries out the pending removals of every account in `seq` order; a removal that fails
    /// stays (said once while its message stays the same) and so do the later ones of its
    /// account. A removal whose account looks alive is dropped only when a list read after the
    /// removal still shows it; a fresh one waits one turn. A removal whose account the source
    /// leaves out without knowing it is gone waits too. Gives whether any removal was done.
    fn removals(
        &self,
        lock: &WriterLock,
        stores: &mut BTreeMap<String, TextStore>,
        pending: &mut Pending,
        all: &[ServerAccount],
    ) -> bool {
        let mut done = false;
        pending.retain(|slug, list| {
            let topic = format!("removal:{slug}");
            while let Some((&seq, &waiting)) = list.first_key_value() {
                let removal = Removal {
                    seq,
                    user_id: waiting.user_id,
                };
                match self.remove(lock, stores, slug, removal, all) {
                    Ok(Outcome::Alive) if waiting.fresh => {
                        self.settled(&topic);
                        if let Some(w) = list.get_mut(&seq) {
                            w.fresh = false;
                        }
                        break;
                    }
                    Ok(Outcome::Wait) => {
                        self.settled(&topic);
                        break;
                    }
                    Ok(Outcome::Alive | Outcome::Done) => {
                        list.pop_first();
                        self.settled(&topic);
                        done = true;
                    }
                    Err(e) => {
                        self.say_once(
                            &topic,
                            format!(
                                "notedown pipeline: account {slug} was removed on the server, \
                                 but its note text cannot be deleted yet: {e}"
                            ),
                        );
                        break;
                    }
                }
            }
            !list.is_empty()
        });
        done
    }

    /// One `account.removed` event of account `slug`: deletes the store when it is of the
    /// removed user id and the server does not list the slug with that id (a crash between
    /// the server's record and its removal leaves the account alive). A slug left out of the
    /// list counts as gone only when the source knows it ([`ServerData::knows_gone`]); else
    /// the removal waits. A store without owner counts as the removed one only on one
    /// machine, and only when the server lists no account of that slug. A missing store is not
    /// created.
    fn remove(
        &self,
        lock: &WriterLock,
        stores: &mut BTreeMap<String, TextStore>,
        slug: &str,
        removal: Removal,
        all: &[ServerAccount],
    ) -> Result<Outcome> {
        let dir = self.setup.local.join(crate::store::PIPELINE_DIR).join(slug);
        match std::fs::symlink_metadata(&dir) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Outcome::Done),
            Err(source) => return Err(Error::Io { path: dir, source }),
        }
        let owner = match stores.get(slug) {
            Some(text) => text.owner()?,
            None => TextStore::open(&self.setup.local, slug)?.owner()?,
        };
        let listed = all.iter().find(|a| a.slug == slug);
        // Whether the store is of the removed account, and whether the list says it lives.
        let (of_removed, alive) = match owner {
            Some(id) => (
                id == removal.user_id,
                listed.is_some_and(|a| a.user_id == id),
            ),
            None => (!self.setup.two_machines, listed.is_some()),
        };
        if !of_removed {
            return Ok(Outcome::Done);
        }
        if alive {
            return Ok(Outcome::Alive);
        }
        if listed.is_none() && !self.setup.source.knows_gone(slug) {
            return Ok(Outcome::Wait);
        }
        stores.remove(slug);
        let destroyed = TextStore::destroy(&self.setup.local, slug, lock)?;
        self.sweep(lock);
        if destroyed {
            self.say(&format!(
                "notedown pipeline: account {slug} was removed on the server; its note text is \
                 deleted"
            ));
        }
        Ok(Outcome::Done)
    }

    /// One pass over all accounts: the due notes whose settle time is over (all of them with
    /// `ignore_settle`), page by page, each page written at once.
    pub fn run_once(&mut self, ignore_settle: bool) -> Result<Progress> {
        // Before the lock, so a run with nothing to do leaves no `pipeline/` folder behind.
        let accounts = match self.followed() {
            Ok(f) => f.served,
            // A new server folder without accounts: nothing to read, not an error.
            Err(e @ Error::NoAccounts(_)) => {
                self.say(&format!("notedown pipeline: {e}"));
                return Ok(Progress::default());
            }
            Err(e) => return Err(e),
        };
        let lock = self.take_lock()?;
        let mut stores = BTreeMap::new();
        let (progress, _) = self.pass(&lock, &mut stores, &accounts, ignore_settle, &|| false)?;
        Ok(progress)
    }

    /// Reads note `note` (a key or a wire path) or, with `None`, every note in scope again,
    /// with the whole page as the change region. Changed text gives normal versions.
    pub fn reocr(&mut self, note: Option<&str>) -> Result<Progress> {
        let served = self.followed()?.served;
        let lock = self.take_lock()?;
        let source = self.setup.source.clone();
        let mut progress = Progress::default();
        let mut found = false;
        let mut stores = BTreeMap::new();
        for account in served {
            let slug = account.slug.clone();
            stores.insert(
                slug.clone(),
                TextStore::open_writer(&self.setup.local, &slug, &lock)?,
            );
            if !self.owner_check(&lock, &mut stores, &account)? {
                continue;
            }
            let text = &stores[&slug];
            let store = AccountStore { text, lock: &lock };
            let now = self.clock.now_ms();
            // Note changes (moves, scope) first, so the states are current.
            due(
                &*source,
                &slug,
                &store,
                &self.setup.cfg,
                &mut self.keywords.borrow_mut(),
                now,
            )?;
            let recs = source.records(&slug)?;
            let digests = source.digests(&slug)?;
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
                if !in_scope(&self.setup.cfg, &r.path, &words) {
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
                    source: &*source,
                    slug: &slug,
                    store,
                    records: &recs.records,
                    digests: &digests,
                    now: &|| clock.now_ms(),
                    mode: Mode::Reocr,
                    told_missing: &self.told_missing,
                };
                let read = note::process(
                    &job,
                    &mut self.engine,
                    &d,
                    &mut progress,
                    &mut |_| {},
                    &|| false,
                );
                if let Err(e) = read {
                    self.clear_work(&lock);
                    return Err(e);
                }
            }
            text.set_last_run(&lock, self.clock.now_ms())?;
        }
        self.clear_work(&lock);
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

    /// The continuous worker: follows the journal of every followed account, each with its
    /// own cursor in the local folder; looks at the records at start, after new journal lines,
    /// when a settle time or retry ends and every 10 minutes, and reads what is due. The model
    /// is unloaded after `model_idle` without work. After each pass it prunes the text log of
    /// every account (at most once a day, spec 2.4). Returns when `stop` says so; it is asked
    /// between pages too.
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
        let lock = self.take_lock()?;
        started();
        // A failing prune is said at most once a day per account.
        let mut prune_warned: BTreeMap<String, u64> = BTreeMap::new();
        let mut follows: BTreeMap<String, Follow> = BTreeMap::new();
        let mut pending: Pending = BTreeMap::new();
        let mut stores = BTreeMap::new();
        let mut look = true;
        let mut next_wake = 0u64;
        loop {
            match self.followed() {
                // Nothing known: no pass, no cursor moves; asked again at the next poll.
                Err(e) => {
                    let message = format!("{}: {e}", self.setup.source.describe());
                    if let Ok(slugs) = crate::setup::local_stores(&self.setup.local, false) {
                        for slug in slugs {
                            self.keep_error(&lock, &slug, message.clone());
                        }
                    }
                    self.say_once("accounts", format!("notedown pipeline: {message}"));
                }
                Ok(followed) => {
                    self.settled("accounts");
                    for slug in &followed.journals {
                        let (changed, removed) = self.follow(slug, &mut follows);
                        look |= changed;
                        for r in removed {
                            pending
                                .entry(slug.clone())
                                .or_default()
                                .entry(r.seq)
                                .or_insert(Waiting {
                                    user_id: r.user_id,
                                    fresh: true,
                                });
                        }
                    }
                    look |= self.removals(&lock, &mut stores, &mut pending, &followed.all);
                    // An account with a removal not done yet is not read.
                    let accounts: Vec<ServerAccount> = followed
                        .served
                        .into_iter()
                        .filter(|a| !pending.contains_key(&a.slug))
                        .collect();
                    let now = self.clock.now_ms();
                    if look || now >= next_wake {
                        look = false;
                        // Taken before the pass, so an event that comes during a long pass is
                        // read next time and not skipped. The cursor never passes a removal
                        // not done yet: a restart reads it again.
                        let seen: Vec<(String, u64)> = followed
                            .journals
                            .iter()
                            .filter_map(|slug| {
                                let read = follows.get(slug)?.read;
                                let held = pending
                                    .get(slug)
                                    .and_then(|p| p.keys().next())
                                    .map_or(read, |first| read.min(first.saturating_sub(1)));
                                Some((slug.clone(), held))
                            })
                            .collect();
                        let result = self.pass(&lock, &mut stores, &accounts, false, &stop);
                        let now = self.clock.now_ms();
                        next_wake = now + RESCAN_MS;
                        match result {
                            Ok((p, unsure)) => {
                                self.prune(&lock, &stores, &mut prune_warned, now);
                                if let Some(t) = p.next_due {
                                    next_wake = next_wake.min(t.max(now));
                                }
                                if !unsure.is_empty() {
                                    next_wake = next_wake.min(now + ERROR_WAIT_MS);
                                }
                                for (slug, seq) in seen {
                                    if !unsure.contains(&slug) {
                                        self.save_cursor(&slug, seq, &mut follows);
                                    }
                                }
                            }
                            Err(Error::ModelMissing) => {
                                next_wake = next_wake.min(self.engine.retry_at.max(now));
                            }
                            Err(e) => {
                                self.say(&format!("notedown pipeline: {e}"));
                                next_wake = now + ERROR_WAIT_MS;
                            }
                        }
                    }
                }
            }
            self.engine
                .unload_if_idle(self.clock.now_ms(), self.setup.cfg.model_idle);
            if stop() {
                return Ok(());
            }
            self.clock.sleep(POLL);
        }
    }

    /// Old page versions go once a day (the prune's own stamp limits it).
    fn prune(
        &self,
        lock: &WriterLock,
        stores: &BTreeMap<String, TextStore>,
        warned: &mut BTreeMap<String, u64>,
        now: u64,
    ) {
        for (slug, text) in stores.iter() {
            if let Err(e) = retention::prune(text, lock, self.setup.cfg.text_keep_days, now)
                && warned
                    .get(slug)
                    .is_none_or(|&at| now.saturating_sub(at) >= DAY_MS)
            {
                self.say(&format!("notedown pipeline: account {slug}: prune: {e}"));
                warned.insert(slug.clone(), now);
            }
        }
    }

    /// Where account `slug` starts in its journal: its cursor; else, on one machine, the old
    /// cursor of all accounts (only read, so an update does not read the whole journal again);
    /// else the start.
    /// A cursor file that does not parse counts as missing; it is written again after the
    /// next good pass.
    fn start(&self, slug: &str) -> Result<Follow> {
        let saved = match cursor::load(&self.setup.local, slug) {
            Err(e @ Error::Corrupt { .. }) => {
                self.say_once(
                    &format!("cursor:{slug}"),
                    format!(
                        "notedown pipeline: account {slug}: the journal cursor cannot be read \
                         ({e}); it is written again after the next pass"
                    ),
                );
                None
            }
            other => other?,
        };
        let read = match saved {
            Some(seq) => seq,
            // An old cursor that cannot be read only costs a full read of the journal.
            None if !self.setup.two_machines => {
                cursor::old(&self.setup.local).ok().flatten().unwrap_or(0)
            }
            None => 0,
        };
        Ok(Follow { read, saved })
    }

    /// Reads the new journal lines of account `slug`, page by page to the end, and tells
    /// whether any of them is about the account; gives the `account.removed` events read too.
    /// A source without an answer leaves the position as it is. A journal that ends before the
    /// position (the server's journal was started again) is read again from the start.
    fn follow(&self, slug: &str, follows: &mut BTreeMap<String, Follow>) -> (bool, Vec<Removal>) {
        let topic = format!("journal:{slug}");
        let mut removed = Vec::new();
        if !follows.contains_key(slug) {
            match self.start(slug) {
                Ok(f) => {
                    follows.insert(slug.to_string(), f);
                }
                Err(e) => {
                    self.say_journal(
                        slug,
                        &topic,
                        format!("notedown pipeline: account {slug}: {e}"),
                    );
                    return (false, removed);
                }
            }
        }
        let f = follows.get_mut(slug).expect("inserted above");
        let mut changed = false;
        loop {
            let batch = match self.setup.source.journal(slug, f.read + 1, JOURNAL_PAGE) {
                Ok(b) => b,
                Err(e) => {
                    self.say_journal(
                        slug,
                        &topic,
                        format!("notedown pipeline: account {slug}: journal: {e}"),
                    );
                    return (changed, removed);
                }
            };
            self.settled(&topic);
            // An end of 0 is "nothing known" (no journal, or a share that is not mounted).
            if batch.end > 0 && batch.end < f.read {
                self.say(&format!(
                    "notedown pipeline: the journal of {slug} ends at {}, before this \
                     pipeline's position {}; reading it again from the start",
                    batch.end, f.read
                ));
                f.read = 0;
                changed = true;
                continue;
            }
            changed |= batch.changed;
            removed.extend(batch.removed);
            f.read = f.read.max(batch.next.saturating_sub(1));
            if !batch.more {
                return (changed, removed);
            }
        }
    }

    /// Says a journal failure of account `slug` once, unless the refusal of its token is said:
    /// a refused token fails every journal read, and the refusal already tells what to do.
    fn say_journal(&self, slug: &str, topic: &str, message: String) {
        if self.said.borrow().contains_key(&format!("token:{slug}")) {
            return;
        }
        self.say_once(topic, message);
    }

    /// Writes the cursor of account `slug` after a good pass, when it moved or has no file.
    fn save_cursor(&self, slug: &str, seq: u64, follows: &mut BTreeMap<String, Follow>) {
        let Some(f) = follows.get_mut(slug) else {
            return;
        };
        if f.saved == Some(seq) {
            return;
        }
        let topic = format!("cursor:{slug}");
        match cursor::write(&self.setup.local, slug, seq) {
            Ok(()) => {
                f.saved = Some(seq);
                self.settled(&topic);
            }
            Err(e) => self.say_once(
                &topic,
                format!("notedown pipeline: account {slug}: journal cursor: {e}"),
            ),
        }
    }

    /// One pass: the due notes of every account in one queue. Notes with text go first, the
    /// one whose settle time ended first at the front; then notes without any text yet (the
    /// first run), newest first, with progress on stderr. An account that fails is reported
    /// (once while it keeps failing) and skipped; a missing model ends the pass. Also gives
    /// the accounts that were not looked at fully (a source error, a broken store, or notes
    /// left by a stop), whose cursors must stay.
    fn pass(
        &mut self,
        lock: &WriterLock,
        stores: &mut BTreeMap<String, TextStore>,
        accounts: &[ServerAccount],
        ignore_settle: bool,
        stop: &dyn Fn() -> bool,
    ) -> Result<(Progress, BTreeSet<String>)> {
        let now = self.clock.now_ms();
        let settle = self.setup.cfg.ocr_settle.as_millis() as u64;
        let source: Arc<dyn ServerData> = self.setup.source.clone();
        let mut progress = Progress::default();
        let mut queue: Vec<(String, DueNote)> = Vec::new();
        let mut recs: BTreeMap<String, (records::Records, Vec<Digest>)> = BTreeMap::new();
        let mut next_due: Option<u64> = None;
        // Per account for its stats: the waiting pages and the next due time.
        let mut waiting: BTreeMap<String, (u64, Option<u64>)> = BTreeMap::new();
        let mut unsure: BTreeSet<String> = BTreeSet::new();
        // A refused token leaves its account out of the pass: its store keeps the refusal.
        for (slug, message) in self.setup.source.refusals() {
            self.keep_error(lock, &slug, message);
        }
        // Only the stores of this pass stay open: no stamp or prune touches another one, and a
        // removed account's folder is never made again.
        stores.retain(|slug, _| accounts.iter().any(|a| &a.slug == slug));
        for account in accounts {
            let slug = account.slug.clone();
            let found = (|| -> Result<Option<Vec<DueNote>>> {
                if !stores.contains_key(&slug) {
                    let text = TextStore::open_writer(&self.setup.local, &slug, lock)?;
                    stores.insert(slug.clone(), text);
                }
                if !self.owner_check(lock, stores, account)? {
                    return Ok(None);
                }
                let text = &stores[&slug];
                let store = AccountStore { text, lock };
                let notes = due(
                    &*source,
                    &slug,
                    &store,
                    &self.setup.cfg,
                    &mut self.keywords.borrow_mut(),
                    now,
                )?;
                if let Some(t) = Errors::load(text.dir())?.next_try(now) {
                    next_due = Some(next_due.map_or(t, |n| n.min(t)));
                    waiting.entry(slug.clone()).or_default().1 = Some(t);
                }
                recs.insert(
                    slug.clone(),
                    (source.records(&slug)?, source.digests(&slug)?),
                );
                Ok(Some(notes))
            })();
            let topic = format!("account:{slug}");
            let notes = match found {
                Ok(Some(n)) => {
                    self.settled(&topic);
                    n
                }
                // Skipped by the owner check (said there): its cursor stays.
                Ok(None) => {
                    self.settled(&topic);
                    stores.remove(&slug);
                    unsure.insert(slug);
                    continue;
                }
                Err(e) => {
                    self.say_once(&topic, format!("notedown pipeline: account {slug}: {e}"));
                    self.keep_error(lock, &slug, e.to_string());
                    // A failed write blocks the store until a repair: open it again next time.
                    stores.remove(&slug);
                    unsure.insert(slug);
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
                    let w = waiting.entry(slug.clone()).or_default();
                    w.0 += d.pages.len() as u64;
                    w.1 = Some(w.1.map_or(ends, |n| n.min(ends)));
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
        for (i, (slug, d)) in queue.iter().enumerate() {
            if broken.contains(slug) {
                continue;
            }
            if stop() {
                // The notes not read, and the one before (a stop between its pages leaves it
                // half read): their accounts keep their cursors.
                for (s, _) in &queue[i.saturating_sub(1)..] {
                    unsure.insert(s.clone());
                }
                break;
            }
            let (Some(text), Some((r, digests))) = (stores.get(slug), recs.get(slug)) else {
                continue;
            };
            let job = NoteJob {
                source: &*source,
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
                Err(Error::ModelMissing) => {
                    self.clear_work(lock);
                    for slug in stores.keys() {
                        self.keep_error(lock, slug, Error::ModelMissing.to_string());
                    }
                    return Err(Error::ModelMissing);
                }
                Err(e) => {
                    self.say_once(
                        &format!("note:{slug}"),
                        format!("notedown pipeline: {}: {e}", d.path),
                    );
                    self.keep_error(lock, slug, format!("{}: {e}", d.path));
                    broken.push(slug.clone());
                }
            }
        }
        for a in accounts {
            if !broken.contains(&a.slug) {
                self.settled(&format!("note:{}", a.slug));
            }
        }
        // A failed write blocks the store until a repair: open it again next pass. Such an
        // account (or one whose record or note bytes the source could not give) keeps its
        // cursor.
        for slug in broken {
            stores.remove(&slug);
            unsure.insert(slug);
        }
        self.clear_work(lock);
        let end = self.clock.now_ms();
        for (slug, text) in stores.iter() {
            let (pages, due) = waiting.get(slug).copied().unwrap_or_default();
            text.set_pass_end(lock, end, pages, due)?;
        }
        progress.next_due = next_due;
        Ok((progress, unsure))
    }
}
