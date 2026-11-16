//! The REST client against a scripted server on loopback, with a fake clock and sleeper.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

use super::test_http::{Scripted, answer, answer_with};
use super::*;
use crate::probe::test_server::free_port;

/// A clock that moves only when the sleeper sleeps, and the sleeps it was asked for.
fn fake_timing() -> (Clock, Sleeper, Arc<Mutex<Vec<Duration>>>) {
    let now = Arc::new(AtomicU64::new(1_763_300_000_000));
    let slept = Arc::new(Mutex::new(Vec::new()));
    let clock_now = now.clone();
    let clock: Clock = Arc::new(move || clock_now.load(Ordering::SeqCst));
    let log = slept.clone();
    let sleeper: Sleeper = Arc::new(move |d: Duration| {
        log.lock().unwrap().push(d);
        now.fetch_add(u64::try_from(d.as_millis()).unwrap(), Ordering::SeqCst);
    });
    (clock, sleeper, slept)
}

fn rest(server: &Scripted, token: Option<&str>) -> (Rest, Arc<Mutex<Vec<Duration>>>) {
    let (clock, sleeper, slept) = fake_timing();
    let rest = Rest::with_timing(&server.base(), token.map(Token::new), clock, sleeper).unwrap();
    (rest, slept)
}

fn err(code: &str) -> String {
    format!(r#"{{"error":"{code}"}}"#)
}

#[test]
fn rest_maps_the_error_codes() {
    let cases: Vec<(u16, String, Option<&str>, RestError)> = vec![
        (401, err("token_refused"), None, RestError::TokenRefused),
        (401, err("login_failed"), None, RestError::LoginFailed),
        (429, err("too_many"), None, RestError::TooMany),
        (
            423,
            err("device_syncing"),
            Some("7"),
            RestError::Syncing {
                retry_after: Duration::from_secs(7),
            },
        ),
        // No usable Retry-After: the server's usual 5 s.
        (
            423,
            err("device_syncing"),
            Some("soon"),
            RestError::Syncing {
                retry_after: RETRY_AFTER,
            },
        ),
        (
            423,
            err("device_syncing"),
            None,
            RestError::Syncing {
                retry_after: RETRY_AFTER,
            },
        ),
        (423, err("scan_hold"), None, RestError::ScanHold),
        (423, err("not_imported"), None, RestError::NotImported),
        (403, err("not_writable"), None, RestError::NotWritable),
        (403, err("read_only_token"), None, RestError::ReadOnlyToken),
        (400, err("bad_name"), None, RestError::BadName),
        (400, err("name_too_long"), None, RestError::BadName),
        (
            409,
            r#"{"error":"name_taken","existing":"A.note"}"#.to_owned(),
            None,
            RestError::NameTaken {
                existing: "A.note".into(),
            },
        ),
        (404, err("not_found"), None, RestError::NotFound),
        (
            412,
            err("precondition_failed"),
            None,
            RestError::PreconditionFailed,
        ),
        (413, err("too_large"), None, RestError::TooLarge),
        (507, err("quota_exceeded"), None, RestError::Full),
        (410, err("account_removed"), None, RestError::AccountRemoved),
        (503, err("store_damaged"), None, RestError::StoreDamaged),
        // A proxy in front of a server that is down answers without a Notedown error.
        (502, "Bad Gateway".to_owned(), None, RestError::Unreachable),
        (504, String::new(), None, RestError::Unreachable),
        (
            409,
            err("not_a_folder"),
            None,
            RestError::Server {
                status: 409,
                code: Some("not_a_folder".into()),
            },
        ),
        // A 423 is only a wait by its code.
        (
            423,
            "{}".to_owned(),
            None,
            RestError::Server {
                status: 423,
                code: None,
            },
        ),
        (
            404,
            err("no_route"),
            None,
            RestError::Server {
                status: 404,
                code: Some("no_route".into()),
            },
        ),
        (
            301,
            String::new(),
            None,
            RestError::Server {
                status: 301,
                code: None,
            },
        ),
    ];
    for (status, body, retry, want) in cases {
        assert_eq!(
            error_for(status, body.as_bytes(), retry),
            want,
            "{status} {body}"
        );
    }

    // The decided texts.
    let texts = [
        (
            RestError::LoginFailed,
            "Wrong e-mail or password, or the account is locked for now.",
        ),
        (RestError::TooMany, "Too many tries. Wait a minute."),
        (
            RestError::Syncing {
                retry_after: RETRY_AFTER,
            },
            "The Supernote is syncing. Try again in a moment.",
        ),
        (
            RestError::NotImported,
            "The Supernote has not been imported yet.",
        ),
        (
            RestError::TokenRefused,
            "The server refused the login. Log in again.",
        ),
        (RestError::Unreachable, "The server does not answer."),
        (RestError::NotWritable, "This folder cannot be changed."),
        (RestError::BadName, "This name is not allowed."),
        (
            RestError::NameTaken {
                existing: "A.note".into(),
            },
            "There is already A.note here.",
        ),
        (RestError::Full, "The account is full."),
        (RestError::TooLarge, "Files up to 1 GB."),
    ];
    for (error, text) in texts {
        assert_eq!(error.to_string(), text);
    }
    // Every text is a plain sentence without a dash of the long kinds.
    // The codes without a variant have their own plain text.
    for code in [
        "is_a_folder",
        "not_a_folder",
        "not_a_file",
        "into_itself",
        "sync_refused",
        "slot_damaged",
        "bad_length",
        "bad_md5",
        "length_required",
        "bad_cursor",
        "missing",
        "no_record",
        "no_route",
        "bad_request",
        "internal",
    ] {
        let text = error_for(409, err(code).as_bytes(), None).to_string();
        assert!(!text.contains("answered"), "{code}: {text}");
        assert!(!text.contains(code), "{code}: {text}");
    }
    assert_eq!(
        error_for(409, err("not_a_folder").as_bytes(), None).to_string(),
        "A file is where a folder should be."
    );
    assert_eq!(
        error_for(500, err("internal").as_bytes(), None).to_string(),
        "The server had an error. See the server's log."
    );
    for error in [
        RestError::ScanHold,
        RestError::ReadOnlyToken,
        RestError::NotFound,
        RestError::PreconditionFailed,
        RestError::AccountRemoved,
        RestError::StoreDamaged,
        RestError::Server {
            status: 409,
            code: Some("not_a_folder".into()),
        },
        RestError::Garbled("x".into()),
        RestError::Failed("x".into()),
    ] {
        let text = error.to_string();
        assert!(text.ends_with('.'), "{text}");
        assert!(
            !text.contains('\u{2014}') && !text.contains('\u{2013}'),
            "{text}"
        );
    }
}

#[test]
fn an_answer_is_mapped_through_the_wire() {
    let server = Scripted::start(vec![
        answer(
            "409 Conflict",
            r#"{"error":"name_taken","existing":"Plan.note"}"#,
        ),
        answer("200 OK", r#"{"slug":"jordi"}"#),
        answer("200 OK", "[1,2"),
        "HTTP/1.1 301 Moved Permanently\r\nlocation: http://example.org/\r\ncontent-length: 0\r\n\
         connection: close\r\n\r\n"
            .to_owned(),
    ]);
    let (rest, _) = rest(&server, None);
    let taken = rest.post_json::<_, Value>("/folders", &json!({"path": "NOTE/Note/plan.note"}));
    assert_eq!(
        taken,
        Err(RestError::NameTaken {
            existing: "Plan.note".into()
        })
    );
    let account: Value = rest.get_json("/account", &[]).unwrap();
    assert_eq!(account["slug"], "jordi");
    let garbled = rest.get_json::<Value>("/account", &[]);
    assert!(matches!(garbled, Err(RestError::Garbled(_))), "{garbled:?}");
    // A redirect is never followed.
    let moved = rest.get_json::<Value>("/account", &[]);
    assert_eq!(
        moved,
        Err(RestError::Server {
            status: 301,
            code: None
        })
    );
    assert_eq!(server.received().len(), 4);
}

#[test]
fn a_call_sends_the_token_json_and_an_encoded_query() {
    let server = Scripted::start(vec![
        answer("200 OK", "{}"),
        answer("201 Created", r#"{"id":4}"#),
        answer("200 OK", "{}"),
    ]);
    let (rest, _) = rest(&server, Some("Secret-1"));
    let _: Value = rest
        .get_json("/entries", &[("path", "NOTE/Note/a b&c.note")])
        .unwrap();
    let made: Value = rest
        .post_json("/folders", &json!({"path": "NOTE/Note/x"}))
        .unwrap();
    assert_eq!(made["id"], 4);
    let _: Value = rest.delete_json("/entries/4").unwrap();

    let get = server.next();
    assert_eq!(get.method, "GET");
    assert_eq!(
        get.target,
        "/rest/v1/entries?path=NOTE%2FNote%2Fa+b%26c.note"
    );
    assert_eq!(get.header("authorization"), Some("Bearer Secret-1"));
    let post = server.next();
    assert_eq!(post.method, "POST");
    assert_eq!(post.target, "/rest/v1/folders");
    assert_eq!(post.header("content-type"), Some("application/json"));
    assert_eq!(post.json(), json!({"path": "NOTE/Note/x"}));
    assert_eq!(post.header("authorization"), Some("Bearer Secret-1"));
    let delete = server.next();
    assert_eq!(delete.method, "DELETE");
    assert_eq!(delete.target, "/rest/v1/entries/4");
}

#[test]
fn device_syncing_waits_by_retry_after() {
    let server = Scripted::start(vec![
        answer_with(
            "423 Locked",
            &[("retry-after", "7")],
            &err("device_syncing"),
        ),
        answer_with(
            "423 Locked",
            &[("retry-after", "3")],
            &err("device_syncing"),
        ),
        answer("201 Created", r#"{"id":9}"#),
    ]);
    let (rest, slept) = rest(&server, Some("t"));
    let made: Value = rest
        .post_json("/folders", &json!({"path": "NOTE/Note/x"}))
        .unwrap();
    assert_eq!(made["id"], 9);
    assert_eq!(
        *slept.lock().unwrap(),
        [Duration::from_secs(7), Duration::from_secs(3)]
    );
}

#[test]
fn device_syncing_gives_up_after_two_minutes() {
    let syncing = answer_with(
        "423 Locked",
        &[("retry-after", "50")],
        &err("device_syncing"),
    );
    let server = Scripted::start(vec![
        syncing.clone(),
        syncing.clone(),
        syncing.clone(),
        syncing,
    ]);
    let (rest, slept) = rest(&server, Some("t"));
    let result = rest.post_json::<_, Value>("/folders", &json!({"path": "NOTE/Note/x"}));
    let Err(error) = result else {
        panic!("a sync that does not end is an error")
    };
    assert_eq!(
        error.to_string(),
        "The Supernote is syncing. Try again in a moment."
    );
    // 50 + 50 + 20: never more than two minutes in all.
    assert_eq!(
        *slept.lock().unwrap(),
        [
            Duration::from_secs(50),
            Duration::from_secs(50),
            Duration::from_secs(20)
        ]
    );
    assert_eq!(server.received().len(), 4);
}

#[test]
fn scan_hold_and_not_imported_are_not_waited_for() {
    let server = Scripted::start(vec![
        answer_with("423 Locked", &[("retry-after", "5")], &err("scan_hold")),
        answer("423 Locked", &err("not_imported")),
        answer("201 Created", "{}"),
    ]);
    let (rest, slept) = rest(&server, Some("t"));
    let body = json!({"path": "NOTE/Note/x"});
    assert_eq!(
        rest.post_json::<_, Value>("/folders", &body),
        Err(RestError::ScanHold)
    );
    assert_eq!(
        rest.post_json::<_, Value>("/folders", &body),
        Err(RestError::NotImported)
    );
    assert!(slept.lock().unwrap().is_empty());
    assert_eq!(server.received().len(), 2);
}

#[test]
fn post_once_never_waits() {
    let server = Scripted::start(vec![
        answer_with(
            "423 Locked",
            &[("retry-after", "5")],
            &err("device_syncing"),
        ),
        answer("200 OK", "{}"),
    ]);
    let (rest, slept) = rest(&server, None);
    let result = rest.post_json_once::<_, Value>("/login", &json!({}));
    assert!(
        matches!(result, Err(RestError::Syncing { .. })),
        "{result:?}"
    );
    assert!(slept.lock().unwrap().is_empty());
    assert_eq!(server.received().len(), 1);
}

#[test]
fn an_unreachable_server_says_so_without_its_address() {
    let base = format!("http://user:hunter2@127.0.0.1:{}/rest/v1", free_port());
    let rest = Rest::new(&base, Some(Token::new("Secret-1"))).unwrap();
    let error = rest.get_json::<Value>("/account", &[]).unwrap_err();
    assert_eq!(error, RestError::Unreachable);
    for text in [error.to_string(), format!("{error:?}")] {
        assert!(
            !text.contains("hunter2") && !text.contains("Secret-1"),
            "{text}"
        );
    }
}

#[test]
fn a_garbled_answer_names_no_value_of_it() {
    // A value of the wrong type would be quoted by serde.
    let server = Scripted::start(vec![answer("200 OK", r#"{"n":"Secret-Value"}"#)]);
    let (rest, _) = rest(&server, None);
    #[derive(serde::Deserialize)]
    struct Number {
        #[allow(dead_code)]
        n: u64,
    }
    let error = rest.get_json::<Number>("/x", &[]).err().unwrap();
    assert!(matches!(error, RestError::Garbled(_)), "{error:?}");
    for text in [error.to_string(), format!("{error:?}")] {
        assert!(!text.contains("Secret-Value"), "{text}");
    }
}
