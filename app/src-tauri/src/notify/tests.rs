//! `decide` over sequences of statuses, with the poller's fields set by hand.

use super::*;

use crate::config::ExistingSetup;
use crate::status::{
    DeviceStatus, InWork, LastError, OcrStatus, PipelineStatus, ServerStatus, Setup,
};
use store::SyncCount;

const MIN: u64 = 60_000;
const NOW: u64 = 1_763_300_000_000;

fn base_status() -> Status {
    Status {
        setup: Setup {
            mode: Mode::Local,
            ready: true,
            line: None,
            note: None,
        },
        server: ServerStatus {
            line: "Runs on this Mac".to_owned(),
            state: Some(ChildState::Running {
                pid: 1,
                since_ms: NOW - 60 * MIN,
            }),
            device_urls: Vec::new(),
        },
        pipeline: PipelineStatus {
            line: "Runs".to_owned(),
            state: ChildState::Running {
                pid: 2,
                since_ms: NOW - 60 * MIN,
            },
        },
        device: DeviceStatus {
            line: None,
            name: Some("Nomad".to_owned()),
            last_sync_ms: Some(NOW - 5 * MIN),
            syncing: false,
            online: true,
            files_changed: None,
        },
        ocr: OcrStatus {
            line: "Caught up".to_owned(),
            in_work: None,
            last_error: None,
            queued: 0,
            pending: 0,
            waiting: 0,
            last_run_ms: Some(NOW - 10 * MIN),
            facts: true,
        },
        agents: Vec::new(),
        attention: Vec::new(),
        existing: ExistingSetup {
            server: true,
            pipeline: true,
        },
        account: None,
        connection: Connection::Reached,
        first_sync_error: None,
    }
}

fn agent(name: &str, seq: u64, notes_behind: u64, acked_at_ms: Option<u64>) -> AgentStatus {
    AgentStatus {
        name: name.to_owned(),
        seq,
        read_at_ms: acked_at_ms,
        acked_at_ms,
        notes_behind,
        more: false,
        behind: String::new(),
    }
}

/// One login's record as it goes through `decide`, a status at a time.
struct Rig {
    record: KeyRecord,
    input: NotifyInput,
    choices: NotificationChoices,
    now: u64,
}

impl Rig {
    fn new() -> Self {
        Self {
            record: KeyRecord::default(),
            input: NotifyInput {
                status: base_status(),
                ocr_new: None,
                import_holds_server: false,
                key: Some("local|jordi".to_owned()),
                data_dir: PathBuf::from("/data"),
                import_mark: None,
            },
            choices: NotificationChoices {
                enabled: true,
                ..NotificationChoices::default()
            },
            now: NOW,
        }
    }

    /// A rig after its first status: the baselines are set.
    fn seen() -> Self {
        let mut rig = Self::new();
        rig.record.last_sync_ms = Some(NOW - 60 * MIN);
        rig.record.cursor = Some(100);
        rig.input.ocr_new = Some((0, Some(10)));
        assert!(rig.run().is_empty());
        rig.input.ocr_new = None;
        rig
    }

    fn run(&mut self) -> Vec<Notice> {
        let (notices, next) = decide(&self.record, &self.input, &self.choices, self.now);
        // The poller's fields stay as they are.
        assert_eq!(next.last_sync_ms, self.record.last_sync_ms);
        assert_eq!(next.cursor, self.record.cursor);
        assert_eq!(next.last_count, self.record.last_count);
        assert_eq!(next.count_failing_since, self.record.count_failing_since);
        for n in &notices {
            assert!(allowed(&n.body), "not a listed text: {}", n.body);
            assert_eq!(n.title, n.kind.title());
        }
        self.record = next;
        notices
    }

    fn bodies(&mut self) -> Vec<String> {
        self.run().into_iter().map(|n| n.body).collect()
    }

    /// The poller counted a sync.
    fn synced(&mut self, at_ms: u64, files: Option<u64>) {
        self.record.last_sync_ms = Some(at_ms);
        self.record.cursor = self.record.cursor.map(|c| c + 1);
        self.record.last_count = Some(SyncCount { at_ms, files });
    }

    /// A pass of the pipeline ended now, with nothing left to do.
    fn pass(&mut self, new_notes: u64, newest: u64) {
        self.input.status.ocr.last_run_ms = Some(self.now);
        self.input.ocr_new = Some((new_notes, Some(newest)));
    }

    fn later(&mut self, ms: u64) {
        self.now += ms;
    }
}

/// A notice holds only the listed texts and the counts.
fn allowed(body: &str) -> bool {
    let fixed = [SYNC_UNKNOWN, SERVER_TEXT, OCR_ERROR_TEXT, OLD_SYNC_TEXT];
    let counted = |suffixes: &[&str]| {
        suffixes.iter().any(|s| {
            body.strip_suffix(s)
                .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        })
    };
    fixed.contains(&body)
        || counted(&[" files changed.", " file changed."])
        || counted(&[" notes are ready.", " note is ready."])
        || body
            .strip_suffix(" read the new notes.")
            .is_some_and(|name| ["claude", "roger"].contains(&name))
}

#[test]
fn request_ids_and_titles() {
    let table = [
        (NoticeKind::Sync, "notedown.sync", "Supernote"),
        (NoticeKind::Ocr, "notedown.ocr", "OCR"),
        (NoticeKind::Agent, "notedown.agent", "Agent"),
        (
            NoticeKind::Problem(PROBLEM_SERVER),
            "notedown.problem.server",
            "Needs attention",
        ),
        (
            NoticeKind::Problem(PROBLEM_OCR),
            "notedown.problem.ocr",
            "Needs attention",
        ),
        (
            NoticeKind::Problem(PROBLEM_OLD_SYNC),
            "notedown.problem.oldsync",
            "Needs attention",
        ),
    ];
    for (kind, id, title) in table {
        assert_eq!(kind.id(), id);
        assert_eq!(kind.title(), title);
    }
}

#[test]
fn without_a_login_nothing_happens() {
    let mut rig = Rig::new();
    rig.input.key = None;
    rig.input.status.attention = vec![Attention::ServerUnreachable];
    assert!(rig.run().is_empty());
    assert_eq!(rig.record, KeyRecord::default());
}

#[test]
fn a_sync_gives_one_notice() {
    let mut rig = Rig::seen();
    rig.synced(NOW - MIN, Some(12));
    let notices = rig.run();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].kind, NoticeKind::Sync);
    assert_eq!(notices[0].body, "12 files changed.");
    assert_eq!(notices[0].target, Target::Library(LibraryAt::All));
    // A second status after it gives none.
    assert!(rig.run().is_empty());
    rig.later(MIN);
    assert!(rig.run().is_empty());

    rig.later(11 * MIN);
    rig.synced(rig.now, Some(1));
    assert_eq!(rig.bodies(), ["1 file changed."]);
    rig.later(11 * MIN);
    rig.synced(rig.now, None);
    assert_eq!(rig.bodies(), [SYNC_UNKNOWN]);
}

#[test]
fn a_sync_without_changed_files_gives_no_notice() {
    let mut rig = Rig::seen();
    rig.synced(NOW - MIN, Some(0));
    assert!(rig.run().is_empty());
    assert_eq!(rig.record.sync_shown_at, Some(NOW - MIN));
    assert_eq!(rig.record.sync_last_notice_ms, None);
}

#[test]
fn syncs_within_ten_minutes_add_up() {
    let mut rig = Rig::seen();
    rig.synced(rig.now, Some(12));
    assert_eq!(rig.bodies(), ["12 files changed."]);
    rig.later(2 * MIN);
    rig.synced(rig.now, Some(3));
    assert!(rig.run().is_empty());
    rig.later(5 * MIN);
    rig.synced(rig.now, Some(4));
    assert!(rig.run().is_empty());
    assert_eq!(rig.record.unshown_files, 7);
    rig.later(4 * MIN);
    rig.synced(rig.now, Some(2));
    assert_eq!(rig.bodies(), ["9 files changed."]);
    assert_eq!(rig.record.unshown_files, 0);
}

#[test]
fn the_first_sight_of_a_sync_is_a_baseline() {
    // The notifier first, then the poller's baseline: the first sync after it is shown.
    let mut rig = Rig::new();
    assert!(rig.run().is_empty());
    assert_eq!(rig.record.sync_shown_at, Some(0));
    rig.record.last_sync_ms = Some(NOW - 60 * MIN);
    rig.record.cursor = Some(100);
    assert!(rig.run().is_empty());
    rig.synced(NOW - MIN, Some(4));
    assert_eq!(rig.bodies(), ["4 files changed."]);

    // The poller's baseline first.
    let mut rig = Rig::new();
    rig.record.last_sync_ms = Some(NOW - 60 * MIN);
    rig.record.cursor = Some(100);
    assert!(rig.run().is_empty());
    assert_eq!(rig.record.sync_shown_at, Some(NOW - 60 * MIN));
    rig.synced(NOW - MIN, Some(4));
    assert_eq!(rig.bodies(), ["4 files changed."]);

    // A count already there (a new notify.json, a login seen before): no notice for it.
    let mut rig = Rig::new();
    rig.synced(NOW - MIN, Some(4));
    assert!(rig.run().is_empty());
    assert_eq!(rig.record.sync_shown_at, Some(NOW - MIN));
}

#[test]
fn the_first_sync_after_the_import_mark_gives_no_notice() {
    // Syncs were shown before the import; the poller pins its fields at the mark.
    let mark = NOW - 30 * MIN;
    let mut rig = Rig::seen();
    rig.synced(NOW - 40 * MIN, Some(2));
    rig.run();
    rig.input.import_mark = Some(mark);
    rig.record.last_sync_ms = Some(mark);
    assert!(rig.run().is_empty());
    rig.synced(NOW - MIN, Some(300));
    assert!(rig.run().is_empty());
    assert_eq!(rig.record.sync_shown_at, Some(NOW - MIN));
    // The next one is a sync like any other.
    rig.later(11 * MIN);
    rig.synced(rig.now, Some(5));
    assert_eq!(rig.bodies(), ["5 files changed."]);

    // A mark at the first sight of the login, before any device synced.
    let mut rig = Rig::new();
    rig.input.import_mark = Some(0);
    rig.record.cursor = Some(100);
    assert!(rig.run().is_empty());
    rig.synced(NOW - MIN, Some(300));
    assert!(rig.run().is_empty());
    rig.later(11 * MIN);
    rig.synced(rig.now, Some(5));
    assert_eq!(rig.bodies(), ["5 files changed."]);
}

#[test]
fn no_notice_for_an_old_event() {
    // The app was not running: the events of 10 minutes after NOW come 35 minutes late.
    let mut rig = Rig::seen();
    rig.input.status.agents = vec![agent("claude", 10, 0, Some(NOW - 40 * MIN))];
    rig.run();
    rig.later(45 * MIN);
    let at = NOW + 10 * MIN;
    rig.synced(at, Some(12));
    rig.input.status.ocr.last_run_ms = Some(at);
    rig.input.ocr_new = Some((3, Some(40)));
    rig.input.status.agents = vec![agent("claude", 20, 0, Some(at))];
    assert!(rig.run().is_empty());
    assert_eq!(rig.record.sync_shown_at, Some(at));
    assert_eq!(rig.record.ocr_text_seq, Some(40));
    assert_eq!(rig.record.ocr_last_run_ms, Some(at));
    assert_eq!(rig.record.consumers["claude"].seq, 20);
    assert_eq!(rig.record.unshown_files, 0);
    assert_eq!(rig.record.unshown_notes, 0);
}

#[test]
fn an_ocr_pass_that_leaves_nothing_to_do_gives_one_notice() {
    let mut rig = Rig::seen();
    assert_eq!(rig.record.ocr_text_seq, Some(10));
    assert_eq!(rig.record.ocr_data_dir, Some(PathBuf::from("/data")));
    assert!(!wants_ocr_count(
        &rig.record,
        &rig.input.status,
        &rig.input.data_dir
    ));
    rig.pass(3, 40);
    assert!(wants_ocr_count(
        &rig.record,
        &rig.input.status,
        &rig.input.data_dir
    ));
    let notices = rig.run();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].body, "3 notes are ready.");
    assert_eq!(notices[0].target, Target::Library(LibraryAt::New));
    assert_eq!(rig.record.ocr_text_seq, Some(40));
    assert!(rig.run().is_empty());
    assert!(!wants_ocr_count(
        &rig.record,
        &rig.input.status,
        &rig.input.data_dir
    ));

    rig.later(11 * MIN);
    rig.pass(1, 41);
    assert_eq!(rig.bodies(), ["1 note is ready."]);
    // A pass without new text gives none.
    rig.later(11 * MIN);
    rig.pass(0, 41);
    assert!(rig.run().is_empty());
}

#[test]
fn ocr_is_not_done_while_pages_wait_or_fail() {
    type Change = fn(&mut OcrStatus);
    let cases: [(&str, Change); 5] = [
        ("queued", |o| o.queued = 1),
        ("pending", |o| o.pending = 2),
        ("waiting", |o| o.waiting = 1),
        ("in work", |o| {
            o.in_work = Some(InWork::default());
        }),
        ("error", |o| {
            o.last_error = Some(LastError {
                text: "boom".to_owned(),
                at_ms: NOW,
            });
        }),
    ];
    for (name, set) in cases {
        let mut rig = Rig::seen();
        rig.pass(3, 40);
        set(&mut rig.input.status.ocr);
        assert!(
            !wants_ocr_count(&rig.record, &rig.input.status, &rig.input.data_dir),
            "{name}"
        );
        assert!(rig.run().is_empty(), "{name}");
        assert_eq!(rig.record.ocr_text_seq, Some(10), "{name}");
    }
}

#[test]
fn two_passes_with_a_queue_between_give_one_notice_with_the_sum() {
    let mut rig = Rig::seen();
    rig.pass(2, 20);
    rig.input.status.ocr.queued = 4;
    rig.input.ocr_new = None;
    assert!(rig.run().is_empty());
    rig.later(3 * MIN);
    rig.pass(5, 30);
    rig.input.status.ocr.queued = 0;
    assert_eq!(rig.bodies(), ["5 notes are ready."]);
    assert_eq!(rig.record.ocr_text_seq, Some(30));
}

#[test]
fn a_done_status_without_the_count_moves_nothing() {
    let mut rig = Rig::seen();
    rig.pass(3, 40);
    rig.input.ocr_new = None;
    let before = rig.record.clone();
    assert!(rig.run().is_empty());
    assert_eq!(rig.record.ocr_text_seq, before.ocr_text_seq);
    assert_eq!(rig.record.ocr_last_run_ms, before.ocr_last_run_ms);
    rig.input.ocr_new = Some((3, Some(40)));
    assert_eq!(rig.bodies(), ["3 notes are ready."]);
}

#[test]
fn ocr_passes_within_ten_minutes_add_up() {
    let mut rig = Rig::seen();
    rig.pass(3, 40);
    assert_eq!(rig.bodies(), ["3 notes are ready."]);
    rig.later(4 * MIN);
    rig.pass(2, 42);
    assert!(rig.run().is_empty());
    assert_eq!(rig.record.unshown_notes, 2);
    rig.later(7 * MIN);
    rig.pass(1, 43);
    assert_eq!(rig.bodies(), ["3 notes are ready."]);
}

#[test]
fn another_data_folder_is_a_baseline() {
    let mut rig = Rig::seen();
    rig.input.data_dir = PathBuf::from("/elsewhere");
    rig.pass(9, 80);
    assert!(wants_ocr_count(
        &rig.record,
        &rig.input.status,
        &rig.input.data_dir
    ));
    assert!(rig.run().is_empty());
    assert_eq!(rig.record.ocr_text_seq, Some(80));
    assert_eq!(rig.record.ocr_data_dir, Some(PathBuf::from("/elsewhere")));
    rig.later(MIN);
    rig.pass(2, 82);
    assert_eq!(rig.bodies(), ["2 notes are ready."]);
}

#[test]
fn an_agent_paging_three_batches_gives_one_notice() {
    let mut rig = Rig::seen();
    rig.input.status.agents = vec![agent("claude", 10, 30, None)];
    // A newly seen consumer is a baseline.
    assert!(rig.run().is_empty());
    for (seq, behind) in [(20, 20), (30, 10)] {
        rig.later(5_000);
        rig.input.status.agents = vec![agent("claude", seq, behind, Some(rig.now))];
        assert!(rig.run().is_empty());
    }
    rig.later(5_000);
    rig.input.status.agents = vec![agent("claude", 40, 0, Some(rig.now))];
    let notices = rig.run();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].body, "claude read the new notes.");
    assert_eq!(notices[0].kind, NoticeKind::Agent);
    assert_eq!(notices[0].target, Target::Settings("agents".to_owned()));
    rig.later(MIN);
    assert!(rig.run().is_empty());
    assert_eq!(next_wake(&rig.record), None);
}

#[test]
fn an_agent_that_acks_part_gives_one_notice_after_a_quiet_poll() {
    let mut rig = Rig::seen();
    rig.input.status.agents = vec![agent("claude", 10, 30, None)];
    rig.run();
    rig.input.status.agents = vec![agent("claude", 15, 25, Some(rig.now))];
    assert!(rig.run().is_empty());
    let rose = rig.now;
    assert_eq!(next_wake(&rig.record), Some(rose + AGENT_QUIET_MS));
    // New notes come in while it reads: still behind, no further rise.
    rig.later(30_000);
    rig.input.status.agents = vec![agent("claude", 15, 27, Some(rose))];
    assert!(rig.run().is_empty());
    rig.later(30_000);
    assert_eq!(rig.bodies(), ["claude read the new notes."]);
    assert_eq!(rig.record.consumers["claude"].seq, 15);
    assert_eq!(next_wake(&rig.record), None);
    rig.later(MIN);
    assert!(rig.run().is_empty());
}

#[test]
fn a_status_without_facts_keeps_the_consumers() {
    let mut rig = Rig::seen();
    rig.input.status.agents = vec![agent("claude", 10, 0, None), agent("roger", 5, 0, None)];
    rig.run();
    let consumers = rig.record.consumers.clone();
    rig.input.status.ocr.facts = false;
    rig.input.status.agents = Vec::new();
    assert!(rig.run().is_empty());
    assert_eq!(rig.record.consumers, consumers);
    rig.input.status.ocr.facts = true;
    rig.input.status.agents = vec![agent("claude", 10, 0, None), agent("roger", 5, 0, None)];
    assert!(rig.run().is_empty());
    // A consumer gone from a status with facts is gone.
    rig.input.status.agents = vec![agent("claude", 10, 0, None)];
    rig.run();
    assert_eq!(rig.record.consumers.keys().collect::<Vec<_>>(), ["claude"]);
}

#[test]
fn a_standing_ocr_error_is_not_shown_again() {
    let mut rig = Rig::seen();
    rig.record.problems.insert(
        PROBLEM_OCR.to_owned(),
        ProblemState {
            active: true,
            last_notice_ms: NOW - 120 * MIN,
            for_sync: None,
        },
    );
    // The app starts: no facts yet, then the same error.
    rig.input.status.ocr.facts = false;
    rig.input.status.connection = Connection::Unknown;
    assert!(rig.run().is_empty());
    assert!(rig.record.problems[PROBLEM_OCR].active);
    rig.input.status.ocr.facts = true;
    rig.input.status.ocr.last_error = Some(LastError {
        text: "token abc123 refused".to_owned(),
        at_ms: NOW,
    });
    rig.input.status.attention = vec![Attention::OcrError];
    assert!(rig.run().is_empty());
}

#[test]
fn a_problem_gives_one_notice_per_thirty_minutes() {
    let mut rig = Rig::seen();
    assert!(!rig.record.problems[PROBLEM_SERVER].active);
    rig.input.status.attention = vec![Attention::ServerUnreachable];
    rig.input.status.connection = Connection::Unreachable;
    let notices = rig.run();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].kind, NoticeKind::Problem(PROBLEM_SERVER));
    assert_eq!(notices[0].body, SERVER_TEXT);
    assert_eq!(notices[0].target, Target::Settings("server".to_owned()));
    assert!(rig.run().is_empty());
    // Not known (no answer yet): it stays.
    rig.input.status.attention = Vec::new();
    rig.input.status.connection = Connection::Unknown;
    assert!(rig.run().is_empty());
    rig.input.status.attention = vec![Attention::ServerUnreachable];
    assert!(rig.run().is_empty());
    // Flapping within 30 minutes: one notice.
    for _ in 0..3 {
        rig.later(5 * MIN);
        rig.input.status.attention = Vec::new();
        rig.input.status.connection = Connection::Reached;
        assert!(rig.run().is_empty());
        rig.input.status.attention = vec![Attention::ServerUnreachable];
        rig.input.status.connection = Connection::Unreachable;
        assert!(rig.run().is_empty());
    }
    rig.later(16 * MIN);
    rig.input.status.attention = Vec::new();
    rig.input.status.connection = Connection::Reached;
    rig.run();
    rig.input.status.attention = vec![Attention::ServerUnreachable];
    assert_eq!(rig.bodies(), [SERVER_TEXT]);
}

#[test]
fn a_problem_at_first_sight_is_a_baseline() {
    let mut rig = Rig::new();
    rig.input.status.attention = vec![Attention::ServerUnreachable, Attention::OcrError];
    rig.input.status.connection = Connection::Unreachable;
    rig.input.status.ocr.last_error = Some(LastError::default());
    assert!(rig.run().is_empty());
    assert!(rig.record.problems[PROBLEM_SERVER].active);
    assert!(rig.record.problems[PROBLEM_OCR].active);
}

#[test]
fn both_ocr_problems_are_one_notice() {
    let mut rig = Rig::seen();
    rig.input.status.attention = vec![Attention::OcrError, Attention::PipelineBackoff];
    rig.input.status.ocr.last_error = Some(LastError::default());
    let notices = rig.run();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].kind, NoticeKind::Problem(PROBLEM_OCR));
    assert_eq!(notices[0].body, OCR_ERROR_TEXT);
    assert_eq!(notices[0].target, Target::Settings("ocr".to_owned()));
    // The pipeline in backoff without facts: not gone.
    rig.input.status.attention = vec![Attention::PipelineBackoff];
    rig.input.status.ocr.facts = false;
    rig.input.status.ocr.last_error = None;
    rig.input.status.pipeline.state = ChildState::Backoff {
        until_ms: NOW,
        last_error: "x".to_owned(),
    };
    assert!(rig.run().is_empty());
    assert!(rig.record.problems[PROBLEM_OCR].active);
}

#[test]
fn server_backoff_and_a_refused_token_only_light_the_dot() {
    let mut rig = Rig::seen();
    rig.input.status.attention = vec![Attention::ServerBackoff, Attention::TokenRefused];
    rig.input.status.connection = Connection::Refused;
    assert!(rig.run().is_empty());
}

#[test]
fn an_old_sync_is_shown_once_per_sync() {
    let mut rig = Rig::seen();
    let old = NOW - 8 * 24 * 60 * MIN;
    rig.input.status.device.last_sync_ms = Some(old);
    rig.input.status.attention = vec![Attention::OldSync];
    let notices = rig.run();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].body, OLD_SYNC_TEXT);
    assert_eq!(notices[0].target, Target::Settings("supernote".to_owned()));
    for _ in 0..3 {
        rig.later(60 * MIN);
        assert!(rig.run().is_empty());
    }
    // No account answer: not known, still once.
    rig.input.status.device.last_sync_ms = None;
    rig.input.status.attention = Vec::new();
    rig.run();
    rig.input.status.device.last_sync_ms = Some(old);
    rig.input.status.attention = vec![Attention::OldSync];
    assert!(rig.run().is_empty());
    // A sync, and 7 days later the next one is old: shown again.
    rig.later(60 * MIN);
    rig.input.status.device.last_sync_ms = Some(old + 60 * MIN);
    assert_eq!(rig.bodies(), [OLD_SYNC_TEXT]);
}

#[test]
fn nothing_while_not_set_up_or_while_an_import_holds_the_server() {
    let mut rig = Rig::seen();
    rig.input.import_holds_server = true;
    rig.input.status.attention = vec![Attention::ServerUnreachable];
    rig.input.status.connection = Connection::Unreachable;
    assert!(rig.run().is_empty());
    assert!(rig.record.problems[PROBLEM_SERVER].active);
    // The problem stays after the job: no notice for it.
    rig.input.import_holds_server = false;
    assert!(rig.run().is_empty());

    let mut rig = Rig::seen();
    rig.input.status.setup.mode = Mode::NotSet;
    rig.input.status.attention = vec![Attention::OcrError];
    rig.input.status.ocr.last_error = Some(LastError::default());
    rig.synced(NOW - MIN, Some(3));
    assert!(rig.run().is_empty());
    assert_eq!(rig.record.sync_shown_at, Some(NOW - MIN));
}

#[test]
fn with_notifications_off_the_record_moves_and_nothing_shows() {
    let mut rig = Rig::seen();
    rig.input.status.agents = vec![agent("claude", 10, 0, None)];
    rig.run();
    rig.choices.enabled = false;
    rig.synced(NOW - MIN, Some(12));
    rig.pass(3, 40);
    rig.input.status.agents = vec![agent("claude", 20, 0, Some(NOW))];
    rig.input.status.attention = vec![Attention::ServerUnreachable];
    rig.input.status.connection = Connection::Unreachable;
    assert!(rig.run().is_empty());
    assert_eq!(rig.record.sync_shown_at, Some(NOW - MIN));
    assert_eq!(rig.record.ocr_text_seq, Some(40));
    assert_eq!(rig.record.consumers["claude"].seq, 20);
    assert!(rig.record.problems[PROBLEM_SERVER].active);
    assert_eq!(rig.record.sync_last_notice_ms, None);
    // Turning them on shows none of it.
    rig.choices.enabled = true;
    rig.later(MIN);
    assert!(rig.run().is_empty());
}

#[test]
fn a_kind_turned_off_moves_its_record_and_clears_its_sum() {
    let mut rig = Rig::seen();
    rig.synced(rig.now, Some(12));
    assert_eq!(rig.bodies(), ["12 files changed."]);
    rig.later(2 * MIN);
    rig.synced(rig.now, Some(3));
    rig.run();
    assert_eq!(rig.record.unshown_files, 3);
    rig.choices.sync = false;
    rig.later(MIN);
    rig.synced(rig.now, Some(4));
    assert!(rig.run().is_empty());
    assert_eq!(rig.record.unshown_files, 0);
    rig.choices.sync = true;
    rig.later(11 * MIN);
    rig.synced(rig.now, Some(1));
    assert_eq!(rig.bodies(), ["1 file changed."]);
}
