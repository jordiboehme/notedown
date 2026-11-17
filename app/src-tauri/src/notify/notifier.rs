//! The notifier thread: it turns each status of the poller into notices.
//!
//! The poller's listener ([`listener`]) only puts the status into a channel. The thread reads
//! the config, the import state and the store, counts the notes the OCR made only when
//! [`wants_ocr_count`] says so, runs [`decide`], saves the notifier's fields, and only then
//! shows the notices. A failed save shows nothing: the notices come again with a later status
//! instead of twice. It is logged once until a save works again, and the next wake waits at
//! least [`SAVE_RETRY_MS`]. When [`next_wake`] names a time, [`decide`] runs again then with the last
//! status, also when no new status came. Each status carries the token key of its login; a
//! status for another login than the config's makes no round.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use super::deliver::{Authorization, Deliver};
use super::store::{KeyRecord, NotifyStore};
use super::{NotifyInput, decide, next_wake, wants_ocr_count};
use crate::config::AppConfig;
use crate::status::Status;
use crate::token::token_key;

/// The shortest wait for a wake time. A wake that `decide` cannot settle yet (an agent waits
/// for its quiet poll while the status has no pipeline facts) runs again at most this often.
pub const WAKE_FLOOR: Duration = Duration::from_secs(5);

/// After a failed save the next wake is at least this far away (60 s), so a disk that stays
/// full does not make a round (and maybe a scan of the notes) every [`WAKE_FLOOR`].
pub const SAVE_RETRY_MS: u64 = 60_000;

/// What goes into the notifier's channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// A status and the token key of the login it was made for (`None` without a login).
    Status {
        status: Box<Status>,
        key: Option<String>,
    },
    /// The app quits: the thread ends without another notice.
    Stop,
}

/// The poller's listener: it only sends the status into the channel, with the token key that
/// `key` gives when the status is handed out.
pub fn listener(
    tx: Sender<Msg>,
    key: impl Fn() -> Option<String> + Send + Sync + 'static,
) -> impl Fn(&Status) + Send + Sync + 'static {
    move |status| {
        let _ = tx.send(Msg::Status {
            status: Box::new(status.clone()),
            key: key(),
        });
    }
}

/// Where the notifier keeps its record. [`NotifyStore`] in the app; a fake in tests.
pub trait Records: Send + Sync {
    /// The record of the login `key` (an empty one when there is none).
    fn key_record(&self, key: &str) -> KeyRecord;
    /// Saves the notifier's fields of `from` into the record of `key`, under the store's lock.
    /// The poller's fields stay as the store has them.
    fn save_notifier(&self, key: &str, from: &KeyRecord) -> io::Result<()>;
}

impl Records for NotifyStore {
    fn key_record(&self, key: &str) -> KeyRecord {
        self.read().keys.get(key).cloned().unwrap_or_default()
    }

    fn save_notifier(&self, key: &str, from: &KeyRecord) -> io::Result<()> {
        self.update(|record| {
            set_notifier_fields(record.keys.entry(key.to_owned()).or_default(), from)
        })
    }
}

/// Copies the notifier's fields of `from` into `into`; the poller's fields of `into` stay.
pub fn set_notifier_fields(into: &mut KeyRecord, from: &KeyRecord) {
    // All fields by name: a new field does not compile until it is sorted in here.
    let KeyRecord {
        last_sync_ms: _,
        cursor: _,
        last_count: _,
        count_failing_since: _,
        sync_shown_at,
        sync_last_notice_ms,
        unshown_files,
        ocr_text_seq,
        ocr_data_dir,
        ocr_last_run_ms,
        ocr_last_notice_ms,
        unshown_notes,
        consumers,
        problems,
    } = from.clone();
    into.sync_shown_at = sync_shown_at;
    into.sync_last_notice_ms = sync_last_notice_ms;
    into.unshown_files = unshown_files;
    into.ocr_text_seq = ocr_text_seq;
    into.ocr_data_dir = ocr_data_dir;
    into.ocr_last_run_ms = ocr_last_run_ms;
    into.ocr_last_notice_ms = ocr_last_notice_ms;
    into.unshown_notes = unshown_notes;
    into.consumers = consumers;
    into.problems = problems;
}

/// What a round reads from the rest of the app.
pub struct Reads {
    pub config: Box<dyn Fn() -> AppConfig + Send>,
    /// `ImportJobs::holds_server`.
    pub holds_server: Box<dyn Fn() -> bool + Send>,
    /// `Notes::notes_newer_than`.
    pub notes_newer_than: Box<dyn Fn(u64) -> (u64, Option<u64>) + Send>,
    /// This Mac's clock, in ms since 1970.
    pub now_ms: Box<dyn Fn() -> u64 + Send>,
}

/// Says the notifier thread to end; shared with `stop_for_good`.
#[derive(Clone)]
pub struct StopNotifier {
    stopped: Arc<AtomicBool>,
    tx: Sender<Msg>,
}

impl StopNotifier {
    /// Ends the thread: no notice after this, also not from a status that waits in the
    /// channel (quitting the children makes statuses that look like problems).
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        let _ = self.tx.send(Msg::Stop);
    }
}

pub struct Notifier {
    records: Arc<dyn Records>,
    deliver: Arc<dyn Deliver>,
    reads: Reads,
    stopped: Arc<AtomicBool>,
    /// The denial was logged; said again only after it was allowed in between.
    denied_said: bool,
    /// A failed save was logged; said again only after a save worked in between.
    save_failed_said: bool,
    floor: Duration,
    /// Writes a line to the log (stderr).
    log: Box<dyn Fn(&str) + Send>,
}

impl Notifier {
    /// A notifier and its stop handle; `tx` is the sending side of its channel.
    pub fn new(
        records: Arc<dyn Records>,
        deliver: Arc<dyn Deliver>,
        reads: Reads,
        tx: Sender<Msg>,
    ) -> (Self, StopNotifier) {
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = StopNotifier {
            stopped: stopped.clone(),
            tx,
        };
        let notifier = Self {
            records,
            deliver,
            reads,
            stopped,
            denied_said: false,
            save_failed_said: false,
            floor: WAKE_FLOOR,
            log: Box::new(|line| eprintln!("{line}")),
        };
        (notifier, stop)
    }

    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Takes the statuses in order until [`Msg::Stop`] or the channel closes. Between two
    /// statuses it wakes at [`next_wake`] and decides again with the last status.
    pub fn run(mut self, rx: Receiver<Msg>) {
        let mut last: Option<(Status, Option<String>)> = None;
        let mut wake: Option<u64> = None;
        loop {
            let msg = match wake {
                None => match rx.recv() {
                    Ok(msg) => Some(msg),
                    Err(_) => return,
                },
                Some(at) => {
                    let wait = Duration::from_millis(at.saturating_sub((self.reads.now_ms)()));
                    match rx.recv_timeout(wait.max(self.floor)) {
                        Ok(msg) => Some(msg),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            };
            match msg {
                Some(Msg::Stop) => return,
                Some(Msg::Status { status, key }) => last = Some((*status, key)),
                None => {}
            }
            if self.stopped() {
                return;
            }
            if let Some((status, key)) = &last {
                wake = self.round(status, key.as_deref());
            }
        }
    }

    /// One status, made for the login `for_key`: decide, save, then show. Gives the next wake
    /// time.
    pub fn round(&mut self, status: &Status, for_key: Option<&str>) -> Option<u64> {
        let config = (self.reads.config)();
        // Without a login nothing happens.
        let key = token_key(&config)?;
        // A status made before a login change (one that waited in the channel, or the last
        // one at a wake) is not for this login: its consumers and problems would seed it.
        if for_key != Some(key.as_str()) {
            return None;
        }
        let record = self.records.key_record(&key);
        let ocr_new = wants_ocr_count(&record, status, &config.data_dir)
            .then(|| (self.reads.notes_newer_than)(record.ocr_text_seq.unwrap_or(0)));
        let import_mark = config
            .setup
            .sync_mark
            .as_ref()
            .filter(|m| m.key == key)
            .map(|m| m.sync_at_mark.unwrap_or(0));
        let input = NotifyInput {
            status: status.clone(),
            ocr_new,
            import_holds_server: (self.reads.holds_server)(),
            key: Some(key.clone()),
            data_dir: config.data_dir.clone(),
            import_mark,
        };
        let now = (self.reads.now_ms)();
        let (notices, next) = decide(&record, &input, &config.notifications, now);
        if let Err(why) = self.records.save_notifier(&key, &next) {
            if !self.save_failed_said {
                (self.log)(&format!(
                    "notedown: notify.json was not saved, no notification shown: {why}"
                ));
                self.save_failed_said = true;
            }
            return next_wake(&record).map(|at| at.max(now.saturating_add(SAVE_RETRY_MS)));
        }
        self.save_failed_said = false;
        let wake = next_wake(&next);
        if notices.is_empty() || self.stopped() {
            return wake;
        }
        if self.deliver.authorization() == Authorization::Denied {
            if !self.denied_said {
                (self.log)("notedown: notifications are turned off in System Settings");
                self.denied_said = true;
            }
            return wake;
        }
        self.denied_said = false;
        for notice in &notices {
            if self.stopped() {
                break;
            }
            self.deliver.show(notice);
        }
        wake
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;
    use std::sync::mpsc;
    use std::time::Instant;

    use super::*;
    use crate::config::{Mode, NotificationChoices, SyncMark};
    use crate::notify::deliver::FakeDeliver;
    use crate::notify::store::SyncCount;
    use crate::notify::tests::{MIN, NOW, base_status};
    use crate::notify::{GAP_MS, Notice};

    const KEY: &str = "local|jordi";

    fn config() -> AppConfig {
        AppConfig {
            mode: Mode::Local,
            account: Some("jordi".to_owned()),
            data_dir: PathBuf::from("/data"),
            notifications: NotificationChoices {
                enabled: true,
                ..NotificationChoices::default()
            },
            ..AppConfig::default()
        }
    }

    /// A record after its baselines, with a counted sync of 12 files not shown yet.
    fn synced() -> KeyRecord {
        KeyRecord {
            last_sync_ms: Some(NOW - MIN),
            cursor: Some(100),
            last_count: Some(SyncCount {
                at_ms: NOW - MIN,
                files: Some(12),
            }),
            sync_shown_at: Some(NOW - 60 * MIN),
            ocr_text_seq: Some(10),
            ocr_data_dir: Some(PathBuf::from("/data")),
            ocr_last_run_ms: base_status().ocr.last_run_ms,
            ..KeyRecord::default()
        }
    }

    /// A store in memory that records each save, with what was shown by then.
    struct FakeRecords {
        record: Mutex<KeyRecord>,
        fail: AtomicBool,
        deliver: Arc<FakeDeliver>,
        /// For each save: how many notices were shown before it.
        saves: Mutex<Vec<usize>>,
    }

    impl Records for FakeRecords {
        fn key_record(&self, key: &str) -> KeyRecord {
            assert_eq!(key, KEY);
            self.record.lock().unwrap().clone()
        }

        fn save_notifier(&self, _key: &str, from: &KeyRecord) -> io::Result<()> {
            self.saves.lock().unwrap().push(self.deliver.shown().len());
            if self.fail.load(Ordering::SeqCst) {
                return Err(io::Error::other("disk full"));
            }
            set_notifier_fields(&mut self.record.lock().unwrap(), from);
            Ok(())
        }
    }

    struct Rig {
        records: Arc<FakeRecords>,
        deliver: Arc<FakeDeliver>,
        counts: Arc<AtomicU64>,
        notifier: Notifier,
        stop: StopNotifier,
        rx: Receiver<Msg>,
        tx: Sender<Msg>,
    }

    fn rig_with(record: KeyRecord, fail: bool, deliver: FakeDeliver, config: AppConfig) -> Rig {
        let deliver = Arc::new(deliver);
        let records = Arc::new(FakeRecords {
            record: Mutex::new(record),
            fail: AtomicBool::new(fail),
            deliver: deliver.clone(),
            saves: Mutex::new(Vec::new()),
        });
        let counts = Arc::new(AtomicU64::new(0));
        let counted = counts.clone();
        let reads = Reads {
            config: Box::new(move || config.clone()),
            holds_server: Box::new(|| false),
            notes_newer_than: Box::new(move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                (3, Some(20))
            }),
            now_ms: Box::new(|| NOW),
        };
        let (tx, rx) = mpsc::channel();
        let (notifier, stop) = Notifier::new(records.clone(), deliver.clone(), reads, tx.clone());
        Rig {
            records,
            deliver,
            counts,
            notifier,
            stop,
            rx,
            tx,
        }
    }

    fn rig(record: KeyRecord) -> Rig {
        rig_with(record, false, FakeDeliver::default(), config())
    }

    /// The poller's listener for the login of [`config`].
    fn send(tx: &Sender<Msg>, status: Status) {
        listener(tx.clone(), || Some(KEY.to_owned()))(&status);
    }

    fn bodies(shown: &[Notice]) -> Vec<&str> {
        shown.iter().map(|n| n.body.as_str()).collect()
    }

    #[test]
    fn the_listener_only_sends() {
        let (tx, rx) = mpsc::channel();
        let listen = listener(tx, || Some(KEY.to_owned()));
        listen(&base_status());
        assert_eq!(
            rx.try_recv(),
            Ok(Msg::Status {
                status: Box::new(base_status()),
                key: Some(KEY.to_owned()),
            })
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn the_record_is_saved_before_a_notice_shows() {
        let mut rig = rig(synced());
        rig.notifier.round(&base_status(), Some(KEY));
        assert_eq!(bodies(&rig.deliver.shown()), ["12 files changed."]);
        // One save, and nothing was shown before it.
        assert_eq!(*rig.records.saves.lock().unwrap(), [0]);
        assert_eq!(
            rig.records.record.lock().unwrap().sync_shown_at,
            Some(NOW - MIN)
        );
        // The same status again shows nothing.
        rig.notifier.round(&base_status(), Some(KEY));
        assert_eq!(rig.deliver.shown().len(), 1);
    }

    #[test]
    fn a_failed_save_shows_nothing() {
        let mut rig = rig_with(synced(), true, FakeDeliver::default(), config());
        rig.notifier.round(&base_status(), Some(KEY));
        assert_eq!(*rig.records.saves.lock().unwrap(), [0]);
        assert!(rig.deliver.shown().is_empty());
    }

    #[test]
    fn without_a_login_nothing_is_read_or_saved() {
        let mut rig = rig_with(
            synced(),
            false,
            FakeDeliver::default(),
            AppConfig {
                account: None,
                ..config()
            },
        );
        assert_eq!(rig.notifier.round(&base_status(), Some(KEY)), None);
        assert!(rig.records.saves.lock().unwrap().is_empty());
        assert!(rig.deliver.shown().is_empty());
    }

    #[test]
    fn a_denial_shows_nothing_but_the_record_moves() {
        let deliver = FakeDeliver::new(Authorization::Denied, Authorization::Denied);
        let mut rig = rig_with(synced(), false, deliver, config());
        rig.notifier.round(&base_status(), Some(KEY));
        assert!(rig.deliver.shown().is_empty());
        assert_eq!(
            rig.records.record.lock().unwrap().sync_shown_at,
            Some(NOW - MIN)
        );
    }

    #[test]
    fn with_notifications_off_the_record_moves_without_a_notice() {
        let mut config = config();
        config.notifications.enabled = false;
        let mut rig = rig_with(synced(), false, FakeDeliver::default(), config);
        rig.notifier.round(&base_status(), Some(KEY));
        assert!(rig.deliver.shown().is_empty());
        assert_eq!(
            rig.records.record.lock().unwrap().sync_shown_at,
            Some(NOW - MIN)
        );
    }

    #[test]
    fn the_notes_are_counted_only_when_the_ocr_is_done() {
        let mut rig = rig(synced());
        let mut busy = base_status();
        busy.ocr.queued = 2;
        busy.ocr.last_run_ms = Some(NOW);
        rig.notifier.round(&busy, Some(KEY));
        assert_eq!(rig.counts.load(Ordering::SeqCst), 0);
        // The pass ended and nothing is left: counted once.
        let mut done = base_status();
        done.ocr.last_run_ms = Some(NOW);
        rig.notifier.round(&done, Some(KEY));
        assert_eq!(rig.counts.load(Ordering::SeqCst), 1);
        assert!(bodies(&rig.deliver.shown()).contains(&"3 notes are ready."));
    }

    #[test]
    fn the_first_sync_after_the_import_mark_shows_nothing() {
        let mut config = config();
        config.setup.sync_mark = Some(SyncMark {
            key: KEY.to_owned(),
            seq: 100,
            at_ms: NOW - 2 * 60 * MIN,
            // The newest sync at the mark is the one shown last: the next is the first after.
            sync_at_mark: Some(NOW - 60 * MIN),
        });
        let mut rig = rig_with(synced(), false, FakeDeliver::default(), config);
        rig.notifier.round(&base_status(), Some(KEY));
        assert!(rig.deliver.shown().is_empty());
    }

    #[test]
    fn a_stop_ends_the_thread_before_a_waiting_status_shows() {
        let rig = rig(synced());
        // A status waits in the channel when the app quits: it makes no round.
        send(&rig.tx, base_status());
        rig.stop.stop();
        rig.notifier.run(rig.rx);
        assert!(rig.deliver.shown().is_empty());
        assert!(rig.records.saves.lock().unwrap().is_empty());
    }

    #[test]
    fn a_stop_ends_a_wait_for_a_far_wake() {
        // An agent waits for its quiet poll: the next wake is far away.
        let record = KeyRecord {
            consumers: [(
                "claude".to_owned(),
                crate::notify::store::ConsumerRecord {
                    seq: 1,
                    pending_seq: Some(2),
                    rose_at: Some(NOW),
                },
            )]
            .into(),
            ..synced()
        };
        let mut rig = rig(record);
        rig.notifier.floor = Duration::from_secs(3600);
        let Rig {
            notifier,
            tx,
            rx,
            stop,
            ..
        } = rig;
        send(&tx, base_status());
        let (done_tx, done_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            notifier.run(rx);
            let _ = done_tx.send(());
        });
        // Let it take the status and start waiting.
        std::thread::sleep(Duration::from_millis(368));
        stop.stop();
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(5)), Ok(()));
        thread.join().unwrap();
    }

    #[test]
    fn withheld_files_show_when_the_gap_ends_without_a_new_status() {
        // A notice was shown just before the start; 3 files wait for the gap's end.
        let start = Instant::now();
        let clock = move || NOW + u64::try_from(start.elapsed().as_millis()).unwrap();
        let record = KeyRecord {
            sync_last_notice_ms: Some(NOW + 200 - GAP_MS),
            unshown_files: 3,
            last_count: Some(SyncCount {
                at_ms: NOW - MIN,
                files: Some(3),
            }),
            sync_shown_at: Some(NOW - MIN),
            ..synced()
        };
        let mut rig = rig(record);
        rig.notifier.reads.now_ms = Box::new(clock);
        rig.notifier.floor = Duration::from_millis(10);
        let Rig {
            notifier,
            deliver,
            tx,
            rx,
            stop,
            ..
        } = rig;
        send(&tx, base_status());
        let thread = std::thread::spawn(move || notifier.run(rx));
        let waited = Instant::now();
        while deliver.shown().is_empty() && waited.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(367));
        }
        assert_eq!(bodies(&deliver.shown()), ["3 files changed."]);
        stop.stop();
        thread.join().unwrap();
    }

    #[test]
    fn the_notifier_keeps_the_pollers_fields_saved_in_between() {
        let dir = tempfile::tempdir().unwrap();
        let store = NotifyStore::load(dir.path());
        store
            .update(|r| {
                r.keys.insert(KEY.to_owned(), synced());
            })
            .unwrap();
        let read = store.key_record(KEY);
        // The poller counts the next sync between the notifier's read and its save.
        store
            .update(|r| {
                let k = r.keys.get_mut(KEY).unwrap();
                k.cursor = Some(140);
                k.last_count = Some(SyncCount {
                    at_ms: NOW,
                    files: Some(5),
                });
            })
            .unwrap();
        let mut next = read;
        next.sync_shown_at = Some(NOW - MIN);
        store.save_notifier(KEY, &next).unwrap();
        let saved = NotifyStore::load(dir.path()).key_record(KEY);
        assert_eq!(saved.cursor, Some(140));
        assert_eq!(saved.last_count.map(|c| c.files), Some(Some(5)));
        assert_eq!(saved.sync_shown_at, Some(NOW - MIN));
    }

    #[test]
    fn a_restart_after_a_notice_does_not_show_it_again() {
        let dir = tempfile::tempdir().unwrap();
        let run = |deliver: &Arc<FakeDeliver>| {
            let store = Arc::new(NotifyStore::load(dir.path()));
            let (tx, _rx) = mpsc::channel();
            let reads = Reads {
                config: Box::new(config),
                holds_server: Box::new(|| false),
                notes_newer_than: Box::new(|_| (0, Some(10))),
                now_ms: Box::new(|| NOW),
            };
            let (mut notifier, _) = Notifier::new(store, deliver.clone(), reads, tx);
            notifier.round(&base_status(), Some(KEY));
        };
        NotifyStore::load(dir.path())
            .update(|r| {
                r.keys.insert(KEY.to_owned(), synced());
            })
            .unwrap();
        let first = Arc::new(FakeDeliver::default());
        run(&first);
        assert_eq!(bodies(&first.shown()), ["12 files changed."]);
        let second = Arc::new(FakeDeliver::default());
        run(&second);
        assert!(second.shown().is_empty());
    }

    #[test]
    fn a_status_for_another_login_makes_no_round() {
        // A status of the old login waits in the channel when the login changes.
        let Rig {
            notifier,
            records,
            deliver,
            rx,
            tx,
            stop,
            ..
        } = rig(synced());
        listener(tx, || Some("local|other".to_owned()))(&base_status());
        drop(stop);
        notifier.run(rx);
        assert!(records.saves.lock().unwrap().is_empty());
        assert!(deliver.shown().is_empty());
        // Nor one made without a login.
        let mut rig = rig(synced());
        assert_eq!(rig.notifier.round(&base_status(), None), None);
        assert!(rig.records.saves.lock().unwrap().is_empty());
        assert!(rig.deliver.shown().is_empty());
    }

    #[test]
    fn a_failing_save_is_logged_once_and_backs_off_the_wake() {
        // An agent waits for its quiet poll since 10 minutes: the wake is already due.
        let record = KeyRecord {
            consumers: [(
                "claude".to_owned(),
                crate::notify::store::ConsumerRecord {
                    seq: 1,
                    pending_seq: Some(2),
                    rose_at: Some(NOW - 10 * MIN),
                },
            )]
            .into(),
            ..synced()
        };
        let mut rig = rig_with(record, true, FakeDeliver::default(), config());
        let said: Arc<Mutex<Vec<String>>> = Arc::default();
        let keep = said.clone();
        rig.notifier.log = Box::new(move |line| keep.lock().unwrap().push(line.to_owned()));
        for _ in 0..3 {
            let wake = rig.notifier.round(&base_status(), Some(KEY));
            assert!(wake.is_some_and(|at| at >= NOW + SAVE_RETRY_MS), "{wake:?}");
        }
        assert_eq!(said.lock().unwrap().len(), 1);
        // A save works, then fails again: said again.
        rig.records.fail.store(false, Ordering::SeqCst);
        rig.notifier.round(&base_status(), Some(KEY));
        rig.records.fail.store(true, Ordering::SeqCst);
        rig.records.record.lock().unwrap().last_count = Some(SyncCount {
            at_ms: NOW,
            files: Some(2),
        });
        rig.notifier.round(&base_status(), Some(KEY));
        assert_eq!(said.lock().unwrap().len(), 2);
    }
}
