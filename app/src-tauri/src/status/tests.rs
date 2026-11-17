//! `status_from` on fixture JSON of `pipeline status` and `GET /account`.

use super::*;

const MIN: u64 = 60_000;
const DAY: u64 = 24 * 60 * MIN;
const NOW: u64 = 1_763_300_000_000;

/// `notedown pipeline status` of an account with one page in work and two consumers.
const FACTS: &str = r#"{
  "account": "jordi", "running": true, "notes": 12, "pagesWithText": 40,
  "pending": 2, "queued": 3, "waiting": 0, "nextDueMs": null,
  "failed": 0, "truncated": 0, "errors": 0, "lastError": null, "lastRunMs": 1763299000000,
  "inWork": {"note": "k1", "path": "NOTE/Note/a.note", "page": 2, "pages": 5,
             "startedMs": 1763299990000},
  "consumers": [
    {"name": "claude", "seq": 40, "ackedAt": 1763299000000, "readAt": 1763299500000,
     "notesBehind": 3, "more": false},
    {"name": "roger", "seq": 2, "ackedAt": null, "readAt": null,
     "notesBehind": 50, "more": true},
    {"name": "done", "seq": 99, "ackedAt": 1763299000000, "readAt": 1763299000000,
     "notesBehind": 0, "more": false}
  ]
}"#;

/// `GET /account` with two devices, the newer sync listed second.
const ACCOUNT: &str = r#"{
  "slug": "jordi", "userId": 1000001, "imported": true, "appDataImported": false,
  "devices": [
    {"serial": "SN1", "name": "Old Nomad", "bound": true, "lastLoginMs": 1763200000000,
     "lastSyncMs": 1763200000000, "syncing": false, "online": false},
    {"serial": "SN2", "name": "Jordis Nomad", "bound": true, "lastLoginMs": 1763290000000,
     "lastSyncMs": 1763290900000, "syncing": true, "online": true},
    {"serial": "SN3", "name": null, "bound": true, "lastLoginMs": null,
     "lastSyncMs": null, "syncing": false, "online": false}
  ]
}"#;

fn facts(json: &str) -> PipelineFacts {
    serde_json::from_str(json).unwrap()
}

fn account(json: &str) -> AccountInfo {
    serde_json::from_str(json).unwrap()
}

fn existing() -> ExistingSetup {
    ExistingSetup {
        server: true,
        pipeline: false,
    }
}

fn running(since_ms: u64) -> ChildState {
    ChildState::Running { pid: 42, since_ms }
}

/// A Local setup where everything runs and answers.
fn local() -> Parts {
    let config = AppConfig {
        mode: Mode::Local,
        account: Some("jordi".into()),
        ..AppConfig::default()
    };
    Parts {
        config,
        config_reset: false,
        existing: existing(),
        server: running(NOW - DAY),
        pipeline: running(NOW - DAY),
        device_urls: vec!["http://192.168.1.20:19072".into()],
        pipeline_status: Some(PipelineCall::Facts(facts(FACTS))),
        account: Some(AccountPoll {
            had_token: true,
            answer: AccountAnswer::Account(account(ACCOUNT)),
            unreachable_since_ms: None,
        }),
    }
}

fn unreachable(since_ms: u64, had_token: bool) -> Option<AccountPoll> {
    Some(AccountPoll {
        had_token,
        answer: AccountAnswer::Unreachable("connection refused".into()),
        unreachable_since_ms: Some(since_ms),
    })
}

#[test]
fn a_local_setup_that_works_has_no_attention() {
    let s = status_from(&local(), NOW);
    assert!(s.setup.ready);
    assert_eq!(s.setup.line, None);
    assert_eq!(s.server.line, "Runs on this Mac");
    assert_eq!(s.server.state, Some(running(NOW - DAY)));
    assert_eq!(s.server.device_urls, vec!["http://192.168.1.20:19072"]);
    assert_eq!(s.pipeline.line, "Runs");
    assert!(s.attention.is_empty(), "{:?}", s.attention);
    assert_eq!(s.existing, existing());
}

#[test]
fn status_without_config_says_set_up() {
    let parts = Parts {
        config: AppConfig::default(),
        server: ChildState::Stopped,
        pipeline: ChildState::Stopped,
        device_urls: vec![],
        pipeline_status: None,
        account: None,
        ..local()
    };
    let s = status_from(&parts, NOW);
    assert_eq!(s.setup.mode, Mode::NotSet);
    assert!(!s.setup.ready);
    assert_eq!(s.setup.line.as_deref(), Some("Notedown is not set up yet"));
    assert_eq!(s.server.line, "Not set up yet");
    assert_eq!(s.server.state, None);
    assert_eq!(s.ocr.line, "Not set up yet");
    assert_eq!(s.device.line.as_deref(), Some("Not set up yet"));
    assert!(s.agents.is_empty());
    assert!(s.attention.is_empty());
    assert_eq!(s.existing, existing(), "the setup offers what it found");
}

#[test]
fn a_reset_config_is_noted() {
    let parts = Parts {
        config: AppConfig::default(),
        config_reset: true,
        ..local()
    };
    let s = status_from(&parts, NOW);
    assert_eq!(s.setup.note.as_deref(), Some(RESET_NOTE));
    assert_eq!(status_from(&local(), NOW).setup.note, None);
}

#[test]
fn no_account_yet_is_set_up_not_error() {
    let mut parts = local();
    parts.config.account = None;
    parts.pipeline_status = Some(PipelineCall::Failed(
        "this data folder has no account; create one with notedown cloud account add".into(),
    ));
    parts.account = Some(AccountPoll {
        had_token: false,
        answer: AccountAnswer::Refused,
        unreachable_since_ms: None,
    });
    let s = status_from(&parts, NOW);
    assert!(!s.setup.ready);
    assert_eq!(s.setup.line.as_deref(), Some("Notedown has no account yet"));
    assert_eq!(s.ocr.line, "OCR starts when there is an account");
    assert!(!s.ocr.line.contains("not available"));
    assert!(s.attention.is_empty(), "{:?}", s.attention);
}

#[test]
fn ocr_lines() {
    // Pages in work: the count and the page.
    let s = status_from(&local(), NOW);
    assert_eq!(s.ocr.line, "5 pages waiting");
    assert_eq!(s.ocr.in_work.as_ref().map(|w| w.page), Some(2));
    assert_eq!((s.ocr.queued, s.ocr.pending), (3, 2));

    let with = |json: serde_json::Value| {
        let mut parts = local();
        let mut f: serde_json::Value = serde_json::from_str(FACTS).unwrap();
        for (k, v) in json.as_object().unwrap() {
            f[k] = v.clone();
        }
        parts.pipeline_status = Some(PipelineCall::Facts(serde_json::from_value(f).unwrap()));
        status_from(&parts, NOW)
    };
    let one = with(serde_json::json!({"queued": 1, "pending": 0}));
    assert_eq!(one.ocr.line, "1 page waiting");

    // The last error wins over the queue, and it is attention.
    let failed = with(
        serde_json::json!({"lastError": {"text": "the model is missing",
                                                         "atMs": 1763299000000u64}}),
    );
    assert_eq!(
        failed.ocr.line,
        "The last OCR pass failed: the model is missing"
    );
    assert_eq!(
        failed.ocr.last_error.as_ref().map(|e| e.at_ms),
        Some(1_763_299_000_000)
    );
    assert_eq!(failed.attention, vec![Attention::OcrError]);

    // Pages of notes that still change.
    let settle = with(serde_json::json!({"queued": 0, "pending": 0, "waiting": 4,
                                          "inWork": null}));
    assert_eq!(settle.ocr.line, "4 pages wait for the note to settle");
    let settle_one = with(serde_json::json!({"queued": 0, "pending": 0, "waiting": 1}));
    assert_eq!(settle_one.ocr.line, "1 page waits for the note to settle");

    let done = with(serde_json::json!({"queued": 0, "pending": 0, "waiting": 0,
                                        "inWork": null}));
    assert_eq!(done.ocr.line, "Caught up");
    assert!(done.attention.is_empty());
}

#[test]
fn ocr_before_the_first_call_and_without_a_connection() {
    let mut parts = local();
    parts.pipeline_status = None;
    assert_eq!(status_from(&parts, NOW).ocr.line, "Checking...");

    parts.pipeline = ChildState::NotConnected;
    let s = status_from(&parts, NOW);
    assert_eq!(s.ocr.line, "Not connected yet");
    assert_eq!(s.pipeline.line, "Not connected yet");
}

#[test]
fn a_failed_pipeline_status_shows_its_error() {
    let mut parts = local();
    parts.pipeline_status = Some(PipelineCall::Failed(
        "no account bob in this data folder; its accounts: jordi".into(),
    ));
    let s = status_from(&parts, NOW);
    assert_eq!(
        s.ocr.line,
        "OCR status is not available: no account bob in this data folder; its accounts: jordi"
    );
    assert!(
        s.setup.ready,
        "the account is set, so this is no setup step"
    );
    assert!(s.agents.is_empty());
    assert!(s.attention.is_empty(), "{:?}", s.attention);
}

#[test]
fn the_newest_device_sync_is_shown() {
    let s = status_from(&local(), NOW);
    assert_eq!(s.device.line, None, "the webview says when it synced");
    assert_eq!(s.device.name.as_deref(), Some("Jordis Nomad"));
    assert_eq!(s.device.last_sync_ms, Some(1_763_290_900_000));
    assert!(s.device.syncing);
    assert!(s.device.online);
}

#[test]
fn no_device_yet() {
    let mut parts = local();
    parts.account = Some(AccountPoll {
        had_token: true,
        answer: AccountAnswer::Account(account(r#"{"slug": "jordi", "userId": 1}"#)),
        unreachable_since_ms: None,
    });
    let s = status_from(&parts, NOW);
    assert_eq!(s.device.line.as_deref(), Some("No Supernote yet"));
    assert_eq!(s.device.last_sync_ms, None);
    assert!(s.attention.is_empty());

    // A device that never synced (or an older server that does not record it).
    parts.account = Some(AccountPoll {
        had_token: true,
        answer: AccountAnswer::Account(account(
            r#"{"slug": "jordi", "userId": 1, "devices": [{"serial": "SN3", "name": "Nomad",
                "lastSyncMs": null}]}"#,
        )),
        unreachable_since_ms: None,
    });
    let s = status_from(&parts, NOW);
    assert_eq!(s.device.line.as_deref(), Some("Not synced yet"));
    assert_eq!(s.device.name.as_deref(), Some("Nomad"));
    assert!(s.attention.is_empty(), "no time is no old sync");
}

#[test]
fn no_token_is_not_attention() {
    let mut parts = local();
    parts.config.account = None;
    parts.account = Some(AccountPoll {
        had_token: false,
        answer: AccountAnswer::Refused,
        unreachable_since_ms: None,
    });
    let s = status_from(&parts, NOW);
    assert_eq!(s.device.line.as_deref(), Some("Not logged in yet"));
    assert!(s.attention.is_empty(), "{:?}", s.attention);
}

#[test]
fn a_refused_token_is_attention() {
    let mut parts = local();
    parts.account = Some(AccountPoll {
        had_token: true,
        answer: AccountAnswer::Refused,
        unreachable_since_ms: None,
    });
    let s = status_from(&parts, NOW);
    assert_eq!(
        s.device.line.as_deref(),
        Some("The server refused the login")
    );
    assert_eq!(s.attention, vec![Attention::TokenRefused]);
}

#[test]
fn another_answer_of_the_server_is_its_own_line() {
    let mut parts = local();
    parts.account = Some(AccountPoll {
        had_token: true,
        answer: AccountAnswer::Failed("the server answered 503".into()),
        unreachable_since_ms: None,
    });
    let s = status_from(&parts, NOW);
    assert_eq!(
        s.device.line.as_deref(),
        Some("The device status is not available: the server answered 503")
    );
    assert_eq!(s.server.line, "Runs on this Mac");
    assert!(s.attention.is_empty());
}

#[test]
fn status_of_an_unreachable_server() {
    // Local: not answering for a minute is no problem yet.
    let mut parts = local();
    parts.account = unreachable(NOW - MIN, true);
    let s = status_from(&parts, NOW);
    assert_eq!(s.device.line.as_deref(), Some("The server does not answer"));
    assert_eq!(s.server.line, "Runs on this Mac");
    assert!(s.attention.is_empty());

    // More than 2 minutes is.
    parts.account = unreachable(NOW - 2 * MIN - 1, true);
    let s = status_from(&parts, NOW);
    assert_eq!(s.server.line, "Runs on this Mac but does not answer");
    assert_eq!(s.attention, vec![Attention::ServerUnreachable]);

    // Exactly 2 minutes is not more than 2 minutes.
    parts.account = unreachable(NOW - 2 * MIN, true);
    assert!(status_from(&parts, NOW).attention.is_empty());

    // The time counts from the start of the server, not from before it.
    parts.account = unreachable(NOW - 10 * MIN, false);
    parts.server = running(NOW - MIN);
    assert!(status_from(&parts, NOW).attention.is_empty());

    // A server the user stopped is not expected to answer.
    parts.server = ChildState::Stopped;
    let s = status_from(&parts, NOW);
    assert_eq!(s.server.line, "Stopped");
    assert_eq!(s.device.line.as_deref(), Some("Waiting for the server"));
    assert!(s.attention.is_empty());

    // Remote: the address does not answer.
    let mut remote = local();
    remote.config.mode = Mode::Remote;
    remote.config.server_url = Some("https://notes.example.org".into());
    remote.account = unreachable(NOW - MIN, true);
    let s = status_from(&remote, NOW);
    assert_eq!(s.server.line, "https://notes.example.org does not answer");
    assert_eq!(s.server.state, None);
    assert!(s.attention.is_empty());
    remote.account = unreachable(NOW - 3 * MIN, true);
    assert_eq!(
        status_from(&remote, NOW).attention,
        vec![Attention::ServerUnreachable]
    );

    // A proxy in front of a server that is down answers 502: the server does not answer.
    remote.account = Some(AccountPoll {
        had_token: true,
        answer: AccountAnswer::Unreachable(
            "the server behind https://notes.example.org/rest/v1 does not answer (502)".into(),
        ),
        unreachable_since_ms: Some(NOW - MIN),
    });
    let s = status_from(&remote, NOW);
    assert_eq!(s.server.line, "https://notes.example.org does not answer");
    assert!(s.attention.is_empty());
    remote.account.as_mut().unwrap().unreachable_since_ms = Some(NOW - 2 * MIN - 1);
    assert_eq!(
        status_from(&remote, NOW).attention,
        vec![Attention::ServerUnreachable]
    );

    // Remote and answering, and before the first poll.
    remote.account = Some(AccountPoll {
        had_token: true,
        answer: AccountAnswer::Account(account(ACCOUNT)),
        unreachable_since_ms: None,
    });
    assert_eq!(
        status_from(&remote, NOW).server.line,
        "Runs on https://notes.example.org"
    );
    remote.account = None;
    let s = status_from(&remote, NOW);
    assert_eq!(s.server.line, "Checking https://notes.example.org...");
    assert_eq!(s.device.line.as_deref(), Some("Checking..."));
}

#[test]
fn the_server_line_shows_no_login_of_the_address() {
    let mut remote = local();
    remote.config.mode = Mode::Remote;
    remote.config.server_url = Some("https://user:secret@notes.example.org".into());
    remote.account = None;
    assert_eq!(
        status_from(&remote, NOW).server.line,
        "Checking https://notes.example.org..."
    );
    remote.account = unreachable(NOW - MIN, true);
    assert_eq!(
        status_from(&remote, NOW).server.line,
        "https://notes.example.org does not answer"
    );
    remote.account = Some(AccountPoll {
        had_token: true,
        answer: AccountAnswer::Account(account(ACCOUNT)),
        unreachable_since_ms: None,
    });
    assert_eq!(
        status_from(&remote, NOW).server.line,
        "Runs on https://notes.example.org"
    );
}

#[test]
fn agents_come_from_the_consumers() {
    let s = status_from(&local(), NOW);
    let names: Vec<_> = s.agents.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["claude", "roger", "done"]);
    let claude = &s.agents[0];
    assert_eq!(claude.read_at_ms, Some(1_763_299_500_000));
    assert_eq!(claude.acked_at_ms, Some(1_763_299_000_000));
    assert_eq!(claude.notes_behind, 3);
    assert_eq!(claude.behind, "3 notes behind");
    assert_eq!(s.agents[1].read_at_ms, None);
    assert!(s.agents[1].more);
    assert_eq!(s.agents[1].behind, "50+ notes behind");
    assert_eq!(s.agents[2].behind, "Up to date");

    let mut parts = local();
    let mut f = facts(FACTS);
    f.consumers[0].notes_behind = 1;
    parts.pipeline_status = Some(PipelineCall::Facts(f));
    assert_eq!(status_from(&parts, NOW).agents[0].behind, "1 note behind");
}

#[test]
fn old_sync_is_attention_after_7_days() {
    let synced = |at: u64| {
        let mut parts = local();
        parts.account = Some(AccountPoll {
            had_token: true,
            answer: AccountAnswer::Account(account(&format!(
                r#"{{"slug": "jordi", "userId": 1, "devices": [{{"serial": "SN2",
                    "lastSyncMs": {at}}}]}}"#
            ))),
            unreachable_since_ms: None,
        });
        status_from(&parts, NOW)
    };
    assert!(synced(NOW - 7 * DAY).attention.is_empty());
    assert_eq!(
        synced(NOW - 7 * DAY - 1).attention,
        vec![Attention::OldSync]
    );
    assert!(synced(NOW - DAY).attention.is_empty());
}

#[test]
fn outside_and_not_connected_are_not_attention() {
    let mut parts = local();
    parts.server = ChildState::Outside {
        what: "server".into(),
    };
    parts.pipeline = ChildState::Outside {
        what: "pipeline".into(),
    };
    let s = status_from(&parts, NOW);
    assert_eq!(s.server.line, OUTSIDE_LINE);
    assert_eq!(s.pipeline.line, OUTSIDE_LINE);
    assert_eq!(
        OUTSIDE_LINE,
        "Notedown runs outside the app. Stop it there, then press Start."
    );
    assert!(s.attention.is_empty(), "{:?}", s.attention);

    let mut remote = local();
    remote.config.mode = Mode::Remote;
    remote.config.server_url = Some("https://notes.example.org".into());
    remote.pipeline = ChildState::NotConnected;
    remote.pipeline_status = Some(PipelineCall::Failed(
        "this data folder has no account; create one with notedown cloud account add".into(),
    ));
    let s = status_from(&remote, NOW);
    assert_eq!(s.pipeline.line, "Not connected yet");
    assert!(s.attention.is_empty(), "{:?}", s.attention);
}

#[test]
fn a_backoff_is_attention() {
    let backoff = |e: &str| ChildState::Backoff {
        until_ms: NOW + 4_000,
        last_error: e.into(),
    };
    let mut parts = local();
    parts.server = backoff("address in use");
    parts.pipeline = backoff("the model is missing");
    let s = status_from(&parts, NOW);
    assert_eq!(
        s.server.line,
        "Stopped with an error: address in use. It starts again soon."
    );
    assert_eq!(
        s.pipeline.line,
        "Stopped with an error: the model is missing. It starts again soon."
    );
    assert_eq!(
        s.attention,
        vec![Attention::ServerBackoff, Attention::PipelineBackoff]
    );
    // An error that ends its sentence gets no second period; an ellipsis stays one.
    parts.server = backoff("The port is taken.");
    parts.pipeline = backoff("it waits...");
    let s = status_from(&parts, NOW);
    assert_eq!(
        s.server.line,
        "Stopped with an error: The port is taken. It starts again soon."
    );
    assert_eq!(
        s.pipeline.line,
        "Stopped with an error: it waits... It starts again soon."
    );
}

#[test]
fn the_child_lines() {
    let mut parts = local();
    for (state, line) in [
        (ChildState::Starting, "Starting..."),
        (ChildState::Stopping, "Stopping..."),
        (ChildState::Stopped, "Stopped"),
    ] {
        parts.pipeline = state;
        assert_eq!(status_from(&parts, NOW).pipeline.line, line);
    }
}

#[test]
fn the_status_serializes_for_the_webview() {
    let v = serde_json::to_value(status_from(&local(), NOW)).unwrap();
    assert_eq!(v["setup"]["mode"], "local");
    assert_eq!(v["server"]["state"]["state"], "running");
    assert_eq!(v["server"]["deviceUrls"][0], "http://192.168.1.20:19072");
    assert_eq!(v["device"]["lastSyncMs"], 1_763_290_900_000u64);
    assert_eq!(v["ocr"]["inWork"]["startedMs"], 1_763_299_990_000u64);
    assert_eq!(v["agents"][1]["notesBehind"], 50);
    assert_eq!(v["attention"], serde_json::json!([]));
    assert_eq!(v["existing"]["server"], true);
    assert_eq!(v["connection"], "reached");
    assert_eq!(
        v["account"],
        serde_json::json!({"imported": true, "appDataImported": false})
    );
    let attention = serde_json::to_value(Attention::ServerUnreachable).unwrap();
    assert_eq!(attention, "serverUnreachable");
}

#[test]
fn account_info_reads_the_import_state() {
    let info = account(ACCOUNT);
    assert_eq!(info.imported, Some(true));
    assert_eq!(info.app_data_imported, Some(false));
    let other =
        account(r#"{"slug": "jordi", "userId": 1, "imported": false, "appDataImported": true}"#);
    assert_eq!(other.imported, Some(false));
    assert_eq!(other.app_data_imported, Some(true));
}

#[test]
fn an_older_server_gives_unknown_import_state() {
    let info = account(r#"{"slug": "jordi", "userId": 1000001, "devices": []}"#);
    assert_eq!(info.imported, None);
    assert_eq!(info.app_data_imported, None);
    let parts = Parts {
        account: Some(AccountPoll {
            had_token: true,
            answer: AccountAnswer::Account(info),
            unreachable_since_ms: None,
        }),
        ..local()
    };
    let s = status_from(&parts, NOW);
    assert_eq!(s.connection, Connection::Reached);
    assert_eq!(
        s.account,
        Some(ImportState {
            imported: None,
            app_data_imported: None
        })
    );
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(
        v["account"],
        serde_json::json!({"imported": null, "appDataImported": null})
    );
}

#[test]
fn connection_follows_the_last_account_answer() {
    let with = |answer: Option<AccountAnswer>| {
        status_from(
            &Parts {
                account: answer.map(|answer| AccountPoll {
                    had_token: true,
                    answer,
                    unreachable_since_ms: Some(NOW),
                }),
                ..local()
            },
            NOW,
        )
    };
    let reached = with(Some(AccountAnswer::Account(account(ACCOUNT))));
    assert_eq!(reached.connection, Connection::Reached);
    assert_eq!(
        reached.account,
        Some(ImportState {
            imported: Some(true),
            app_data_imported: Some(false)
        })
    );
    for (answer, connection) in [
        (Some(AccountAnswer::Refused), Connection::Refused),
        (
            Some(AccountAnswer::Unreachable("connection refused".into())),
            Connection::Unreachable,
        ),
        (
            Some(AccountAnswer::Failed("the server answered 500".into())),
            Connection::Failed,
        ),
        (None, Connection::Unknown),
    ] {
        let s = with(answer.clone());
        assert_eq!(s.connection, connection, "{answer:?}");
        assert_eq!(s.account, None, "{answer:?}");
    }
    // Without a mode nothing is polled.
    let not_set = status_from(
        &Parts {
            config: AppConfig::default(),
            account: None,
            ..local()
        },
        NOW,
    );
    assert_eq!(not_set.connection, Connection::Unknown);
    assert_eq!(not_set.account, None);
    let names: Vec<serde_json::Value> = [
        Connection::Unknown,
        Connection::Reached,
        Connection::Refused,
        Connection::Unreachable,
        Connection::Failed,
    ]
    .iter()
    .map(|c| serde_json::to_value(c).unwrap())
    .collect();
    assert_eq!(
        names,
        ["unknown", "reached", "refused", "unreachable", "failed"]
    );
}
