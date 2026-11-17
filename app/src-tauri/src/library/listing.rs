//! Listing folders page by page and following their changes (docs/rest-api.md, "Paging" and
//! "Following changes").
//!
//! Calls run outside the cache's lock; what they bring is merged under it (see `cache.rs`).
//! The follower reads `GET /events` every 2 s while the library window is visible and not at
//! all while it is hidden. When the window shows again and the last read is older than a
//! minute, the listed folders are listed again instead of read up. While the window shows the
//! recycle bin, the follower also runs when no folder is listed: a delete from elsewhere may
//! have changed the bin.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Weak};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::cache::{Cache, Continue, EventPage, GONE, Key, Listed, TreePage};
use super::{Entry, EntryKind, Inner, Library, LibraryError, LibraryEvent, Session, lock};
use crate::rest::RestError;

/// How often the follower reads the events while the window is visible.
pub const FOLLOW_EVERY: Duration = Duration::from_secs(2);
/// A window shown again after a longer pause lists its folders again.
pub const RELIST_AFTER_MS: u64 = 60_000;

/// What the follower knows of the window and its last read.
#[derive(Debug, Default)]
pub struct Follow {
    /// The window is visible; shared with the follower's thread.
    visible: Arc<AtomicBool>,
    /// The window was shown since the last step.
    shown: bool,
    /// The clock at the last read of the events or the last listing again.
    last_poll: Option<u64>,
    /// `account.removed` or a 410: no more reads until the next binding.
    stopped: bool,
    /// The binding these values belong to: an answer of an older one stops nothing.
    generation: u64,
    /// The `seqs` of the app's own writes that the follower has not read yet: their deletes
    /// were announced to the bin view already.
    pub(super) own: std::collections::BTreeSet<u64>,
    /// The window shows the recycle bin.
    bin_shown: bool,
    /// Where the events are read on from while no folder is listed: the `next` of the last
    /// page read. `None` until a page was read, and after a pause or a step that followed
    /// nothing.
    bin_from: Option<u64>,
}

/// An item of the recycle bin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecycleItem {
    pub key: String,
    pub path: String,
    pub name: String,
    pub kind: EntryKind,
    pub deleted_at: i64,
    pub size: u64,
    pub files: Option<u64>,
}

/// A page of the recycle bin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecyclePage {
    pub items: Vec<RecycleItem>,
    pub next: Option<String>,
}

/// A continuation page that tells the app to list the folder again from the first page: the
/// folder was removed or replaced, or the server does not take the cursor any more.
fn starts_again(e: &RestError) -> bool {
    match e {
        RestError::NotFound => true,
        RestError::Server { status, code } => matches!(
            (status, code.as_deref()),
            (409, Some("not_a_folder")) | (400, Some("bad_cursor"))
        ),
        _ => false,
    }
}

/// A first page that finds no folder there.
fn not_there(e: &RestError) -> bool {
    matches!(e, RestError::NotFound)
        || matches!(e, RestError::Server { status: 409, code } if code.as_deref() == Some("not_a_folder"))
}

impl Library {
    /// Runs `change` on the cache and sends the events it made, under the cache's lock.
    fn with_cache<T>(&self, change: impl FnOnce(&mut Cache, &mut Vec<LibraryEvent>) -> T) -> T {
        let mut cache = lock(&self.inner.cache);
        let mut out = Vec::new();
        let answer = change(&mut cache, &mut out);
        for event in out {
            (self.inner.emit)(event);
        }
        answer
    }

    /// A new binding: the cache is dropped and the follower starts over (the window's
    /// visibility stays).
    pub(super) fn reset_listing(&self, generation: u64) {
        lock(&self.inner.cache).clear(generation);
        let mut follow = lock(&self.inner.follow);
        follow.shown = false;
        follow.last_poll = None;
        follow.stopped = false;
        follow.generation = generation;
        follow.own.clear();
        follow.bin_shown = false;
        follow.bin_from = None;
    }

    /// The account is gone (`account.removed` or a 410): no more reads until the next
    /// binding. An answer that reached an older binding stops nothing.
    fn stop_following(&self, session: &Session) {
        let mut follow = lock(&self.inner.follow);
        if follow.generation == session.generation() {
            follow.stopped = true;
        }
    }

    /// One page of `GET /tree`.
    fn tree_page(
        &self,
        session: &Session,
        key: &Key,
        cursor: Option<&str>,
    ) -> Result<TreePage, RestError> {
        let mut query = vec![("path", key.path.as_str())];
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor));
        }
        if key.folders_only {
            query.push(("kind", "folder"));
        }
        let result = session.rest().get_json::<TreePage>("/tree", &query);
        self.settle(session, &result);
        result
    }

    /// Lists the folder at `path`: the first page without `cursor`, else the page after it.
    /// The answer is the whole folder as loaded so far, with the cursor of its next page. A
    /// cursor that is not the folder's next one answers the folder as it is. `folders_only`
    /// lists only the child folders (the tree). Blocking: run it off the main thread.
    pub fn list(
        &self,
        path: &str,
        cursor: Option<&str>,
        folders_only: bool,
    ) -> Result<Listed, LibraryError> {
        let session = self.session()?;
        let generation = session.generation();
        let key = Key::new(path, folders_only);
        if let Some(cursor) = cursor {
            let next = self.with_cache(|c, out| {
                (c.generation() == generation).then(|| c.begin_next(generation, &key, cursor, out))
            });
            match next.ok_or(LibraryError::Changed)? {
                Continue::Current(listed) => return Ok(listed),
                Continue::FirstPage => {}
                Continue::Fetch(ticket) => match self.tree_page(&session, &key, Some(cursor)) {
                    Ok(page) => {
                        let merged = self.with_cache(|c, out| {
                            (c.generation() == generation)
                                .then(|| c.merge_next(ticket, &key, cursor, page, out))
                        });
                        if let Some(listed) = merged.ok_or(LibraryError::Changed)? {
                            return Ok(listed);
                        }
                    }
                    Err(e) if starts_again(&e) => {
                        self.with_cache(|c, _| c.drop_folder(generation, &key));
                    }
                    Err(e) => {
                        self.with_cache(|c, out| c.failed(ticket, &key, &e.to_string(), out));
                        return Err(LibraryError::Rest(e));
                    }
                },
            }
        }
        self.first_page(&session, &key, true)
    }

    /// Lists `key` from its first page. A folder that is not there is dropped and the window
    /// hears it. A binding that changes before or during the call answers
    /// [`LibraryError::Changed`].
    fn first_page(
        &self,
        session: &Session,
        key: &Key,
        by_window: bool,
    ) -> Result<Listed, LibraryError> {
        let generation = session.generation();
        let ticket = self
            .with_cache(|c, _| {
                (c.generation() == generation).then(|| c.begin_first(generation, key))
            })
            .ok_or(LibraryError::Changed)?;
        match self.tree_page(session, key, None) {
            Ok(page) => {
                let fallback = Listed {
                    folder: super::FolderState {
                        path: key.path.clone(),
                        entries: page
                            .entries
                            .iter()
                            .filter(|e| !key.folders_only || e.kind == EntryKind::Folder)
                            .cloned()
                            .collect(),
                        complete: page.next.is_none(),
                        loading: false,
                        error: None,
                    },
                    next: page.next.clone(),
                };
                let merged = self.with_cache(|c, out| {
                    (c.generation() == generation)
                        .then(|| c.merge_first(ticket, key, page, by_window, out))
                });
                // A newer first page of the same listing won and was dropped since: this page
                // is answered, not kept.
                Ok(merged.ok_or(LibraryError::Changed)?.unwrap_or(fallback))
            }
            Err(e) => {
                if not_there(&e) {
                    self.with_cache(|c, out| c.gone(generation, key, GONE, out));
                }
                Err(LibraryError::Rest(e))
            }
        }
    }

    /// One file or folder by its id. Blocking: run it off the main thread.
    pub fn entry(&self, id: u64) -> Result<Entry, LibraryError> {
        self.call(|rest| rest.get_json::<Entry>(&format!("/entries/{id}"), &[]))
    }

    /// A page of the recycle bin, the first one without `cursor`. Blocking: run it off the
    /// main thread.
    pub fn recycle(&self, cursor: Option<&str>) -> Result<RecyclePage, LibraryError> {
        let query: Vec<(&str, &str)> = cursor.map(|c| ("cursor", c)).into_iter().collect();
        self.call(|rest| rest.get_json::<RecyclePage>("/recycle", &query))
    }

    /// The library window was shown or hidden. It never blocks: the follower's thread does
    /// the work.
    pub fn set_visible(&self, visible: bool) {
        {
            let mut follow = lock(&self.inner.follow);
            let was = follow.visible.swap(visible, Ordering::SeqCst);
            if visible && !was {
                follow.shown = true;
            }
        }
        self.wake();
    }

    /// The app wrote and the server answered `seqs`. The follower reads them at once; they are
    /// applied in order like every other event, and the window hears of each change once (a
    /// folder is sent only when it changed).
    pub fn wrote(&self, seqs: &[u64]) {
        if !seqs.is_empty() {
            lock(&self.inner.follow).own.extend(seqs);
            self.wake();
        }
    }

    /// Whether `page` may have changed the recycle bin: a `file.deleted` or `folder.deleted`
    /// that is not the app's own (the window heard of those at the write), or a gap. The own
    /// `seqs` that the page passed are forgotten.
    fn bin_changed(&self, page: &EventPage) -> bool {
        let mut follow = lock(&self.inner.follow);
        let mut changed = page.gap;
        for event in &page.events {
            let own = follow.own.remove(&event.seq);
            if !own && matches!(event.kind.as_str(), "file.deleted" | "folder.deleted") {
                changed = true;
            }
        }
        follow.own.retain(|seq| *seq >= page.next);
        changed
    }

    fn wake(&self) {
        if let Some(waker) = lock(&self.inner.waker).as_ref() {
            let _ = waker.send(());
        }
    }

    /// One step of the follower: nothing while the window is hidden; after a show that comes
    /// more than a minute after the last read, the listed folders are listed again; else the
    /// events since the lowest `seq` of the listed folders are read and applied. Blocking.
    pub fn follow_step(&self) {
        let _one_at_a_time = lock(&self.inner.stepping);
        let now = (self.inner.clock)();
        let relist = {
            let mut follow = lock(&self.inner.follow);
            if !follow.visible.load(Ordering::SeqCst) || follow.stopped {
                return;
            }
            let shown = std::mem::take(&mut follow.shown);
            shown
                && follow
                    .last_poll
                    .is_some_and(|t| now.saturating_sub(t) > RELIST_AFTER_MS)
        };
        let Ok(session) = self.session() else {
            return;
        };
        let generation = session.generation();
        // Dropped listings whose first page failed before, and waiting ones whose wait is over.
        let due = lock(&self.inner.cache).due(now);
        self.relist(&session, due, true, now);
        if relist {
            let keys = self.with_cache(|c, _| c.relist_keys(generation));
            self.relist(&session, keys, false, now);
            self.polled(now);
            // Deletes of the pause are not read, so the bin may have changed; the own seqs of
            // the pause are not read either.
            {
                let mut follow = lock(&self.inner.follow);
                follow.own.clear();
                follow.bin_from = None;
            }
            (self.inner.emit)(LibraryEvent::Recycle);
            return;
        }
        // A `recycle` is sent at most once a step.
        let mut sent = false;
        loop {
            if session.is_cancelled() {
                return;
            }
            let folder_seq = lock(&self.inner.cache).min_seq();
            let from = match folder_seq {
                Some(seq) => seq + 1,
                None => match self.bin_from(&session) {
                    Ok(Some((from, fresh))) => {
                        // The bin may have changed since the window listed it: said at once,
                        // so a read that fails next does not lose it.
                        if fresh && !sent {
                            (self.inner.emit)(LibraryEvent::Recycle);
                            sent = true;
                        }
                        from
                    }
                    Ok(None) => {
                        self.polled(now);
                        return;
                    }
                    Err(()) => return,
                },
            };
            let from_text = from.to_string();
            let Some(page) = self.read_events(&session, &[("from", from_text.as_str())]) else {
                return;
            };
            self.polled(now);
            lock(&self.inner.follow).bin_from = Some(page.next);
            if self.bin_changed(&page) && !sent {
                (self.inner.emit)(LibraryEvent::Recycle);
                sent = true;
            }
            if folder_seq.is_none() {
                // Only the bin is followed: nothing to apply.
                if page.events.iter().any(|e| e.kind == "account.removed") {
                    self.stop_following(&session);
                    return;
                }
                if !page.more {
                    return;
                }
                continue;
            }
            let seq = from - 1;
            if page.gap {
                // A listing that cannot be read now waits and is tried again later; the others
                // are followed meanwhile.
                let keys = self.with_cache(|c, _| c.relist_keys(generation));
                self.relist(&session, keys, true, now);
                return;
            }
            let applied = self.with_cache(|c, out| c.apply(generation, seq + 1, &page, out));
            let mut fetched = Vec::new();
            for (id, seq) in &applied.wanted {
                if session.is_cancelled() {
                    return;
                }
                let result = session
                    .rest()
                    .get_json::<Entry>(&format!("/entries/{id}"), &[]);
                self.settle(&session, &result);
                if let Ok(entry) = result {
                    fetched.push((entry, *seq));
                }
            }
            if !fetched.is_empty() {
                self.with_cache(|c, out| c.insert_fetched(generation, applied.end, fetched, out));
            }
            self.relist(&session, applied.relist, false, now);
            if applied.stop {
                self.stop_following(&session);
                return;
            }
            if !page.more {
                return;
            }
        }
    }

    /// One page of `GET /events`; `None` when it cannot be read (an account that is gone stops
    /// the follower).
    fn read_events(&self, session: &Session, query: &[(&str, &str)]) -> Option<EventPage> {
        let result = session.rest().get_json::<EventPage>("/events", query);
        self.settle(session, &result);
        match result {
            Ok(page) => Some(page),
            Err(RestError::AccountRemoved) => {
                self.stop_following(session);
                None
            }
            Err(_) => None,
        }
    }

    /// Where the follower reads on from while no folder is listed: `None` when the bin is not
    /// shown either (the place is then forgotten). Without a place yet, the end of the journal
    /// is read first (`fresh`). `Err` when that read fails.
    fn bin_from(&self, session: &Session) -> Result<Option<(u64, bool)>, ()> {
        {
            let mut follow = lock(&self.inner.follow);
            if !follow.bin_shown {
                follow.bin_from = None;
                return Ok(None);
            }
            if let Some(from) = follow.bin_from {
                return Ok(Some((from, false)));
            }
        }
        // Only `end` of this page counts: its events are old.
        let page = self
            .read_events(session, &[("from", "0"), ("limit", "1")])
            .ok_or(())?;
        let from = page.end + 1;
        lock(&self.inner.follow).bin_from = Some(from);
        Ok(Some((from, true)))
    }

    fn polled(&self, now: u64) {
        lock(&self.inner.follow).last_poll = Some(now);
    }

    /// Lists `keys` again from their first page; the new page replaces the folder only when it
    /// comes. One that is not there anymore is dropped; for one that cannot be read now see
    /// [`Cache::relist_failed`] (`gap`: its old `seq` is not valid).
    fn relist(&self, session: &Session, keys: Vec<Key>, gap: bool, now: u64) {
        let generation = session.generation();
        for key in keys {
            if session.is_cancelled() {
                return;
            }
            match self.first_page(session, &key, false) {
                Ok(_) => {}
                Err(LibraryError::Rest(e)) if !not_there(&e) => {
                    let text = e.to_string();
                    self.with_cache(|c, out| {
                        c.relist_failed(generation, &key, &text, gap, now, out);
                    });
                }
                Err(LibraryError::Changed) => return,
                Err(_) => {}
            }
        }
    }

    /// Starts the follower's thread: a step every `every` while the window is visible, and
    /// one at each wake (a show, a hide, a write). It holds only a weak handle, so it ends when
    /// the library is dropped.
    pub fn start_following(&self, every: Duration) -> std::thread::JoinHandle<()> {
        let (tx, rx) = channel::<()>();
        let visible = lock(&self.inner.follow).visible.clone();
        *lock(&self.inner.waker) = Some(tx);
        let weak = Arc::downgrade(&self.inner);
        std::thread::spawn(move || follow_loop(&weak, &rx, &visible, every))
    }

    /// The listings the window shows (the folder in the main area and the open folders of the
    /// tree). The others are dropped and no longer followed; a gap or a long pause lists only
    /// these again.
    pub fn set_shown(&self, keys: &[Key]) {
        lock(&self.inner.cache).set_shown(keys);
    }

    /// Whether the window shows the recycle bin: while it does, the follower also runs when no
    /// folder is listed, so a delete from elsewhere sends `recycle`.
    pub fn set_bin_shown(&self, shown: bool) {
        lock(&self.inner.follow).bin_shown = shown;
    }
}

fn follow_loop(weak: &Weak<Inner>, rx: &Receiver<()>, visible: &AtomicBool, every: Duration) {
    loop {
        let woke = if visible.load(Ordering::SeqCst) {
            rx.recv_timeout(every)
        } else {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        if woke == Err(RecvTimeoutError::Disconnected) {
            return;
        }
        // Wakes that piled up during a step make one step.
        while rx.try_recv().is_ok() {}
        let Some(inner) = weak.upgrade() else {
            return;
        };
        Library { inner }.follow_step();
    }
}
