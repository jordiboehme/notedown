use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use super::*;
use crate::config;
use crate::process::{run_with_password, spawn_program};
use crate::rest::test_http::{Scripted, answer};
use crate::test_procs::{gone, kill_in, left_in, tagged_sleep};
use crate::token::MemoryStore;

const SECRET: &str = "secret";

/// A fake sidecar: it writes each of its arguments as a line to `dir/argv`, then runs `body`.
/// `$D` is `dir`, `$S` this test's own sleep (see [`tagged_sleep`]).
fn sidecar(dir: &Path, body: &str) -> SpawnFn {
    let d = dir.display();
    let s = tagged_sleep(dir);
    let s = s.display();
    let script = format!(
        r#"#!/bin/sh
D="{d}"
S="{s}"
for a in "$@"; do printf '%s\n' "$a"; done > "$D/argv"
{body}
"#
    );
    let path = dir.join("notedown");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    spawn_program(path)
}

/// The arguments of the last call; `None` when the sidecar never ran.
fn argv(dir: &Path) -> Option<Vec<String>> {
    let text = fs::read_to_string(dir.join("argv")).ok()?;
    Some(text.lines().map(str::to_owned).collect())
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

/// Reads stdin into `$D/stdin` and succeeds.
const READS: &str = r#"cat > "$D/stdin""#;

#[test]
fn run_with_password_writes_one_line_to_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let spawn = sidecar(dir.path(), READS);
    let args = strings(&["cloud", "account", "--password-stdin"]);
    let output = run_with_password(
        &spawn,
        &args,
        &Password::new(SECRET),
        Duration::from_secs(10),
    )
    .unwrap();
    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(dir.path().join("stdin")).unwrap(),
        "secret\n"
    );
    let argv = argv(dir.path()).unwrap();
    assert_eq!(argv, args);
    assert!(!argv.iter().any(|a| a.contains(SECRET)));
}

#[test]
fn run_with_password_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let spawn = sidecar(dir.path(), r#"exec "$S" 330"#);
    let start = Instant::now();
    let result = run_with_password(
        &spawn,
        &strings(&["connect"]),
        &Password::new(SECRET),
        Duration::from_millis(300),
    );
    let took = start.elapsed();
    let left = left_in(dir.path());
    kill_in(dir.path());
    assert_eq!(result.unwrap_err(), "it did not answer in time");
    assert!(took < Duration::from_secs(3), "took {took:?}");
    assert!(gone(dir.path()), "left: {left}");
}

/// A shell that runs the sleep as its child (no exec): the sleep holds stdout and stderr.
const SLEEPS_AS_A_CHILD: &str = r#""$S" 330
echo done"#;

#[test]
fn run_with_password_ends_a_grandchild_too() {
    let dir = tempfile::tempdir().unwrap();
    let spawn = sidecar(dir.path(), SLEEPS_AS_A_CHILD);
    let start = Instant::now();
    let result = run_with_password(
        &spawn,
        &strings(&["connect"]),
        &Password::new(SECRET),
        Duration::from_millis(300),
    );
    let took = start.elapsed();
    let left = left_in(dir.path());
    let ended = gone(dir.path());
    kill_in(dir.path());
    assert_eq!(result.unwrap_err(), "it did not answer in time");
    assert!(took < Duration::from_secs(3), "took {took:?}");
    assert!(ended, "left: {left}");
}

#[test]
fn run_bounded_ends_a_grandchild_too() {
    let dir = tempfile::tempdir().unwrap();
    let spawn = sidecar(dir.path(), SLEEPS_AS_A_CHILD);
    let start = Instant::now();
    let result = crate::process::run_bounded(
        &spawn,
        &strings(&["pipeline", "status"]),
        Duration::from_millis(300),
    );
    let took = start.elapsed();
    let left = left_in(dir.path());
    let ended = gone(dir.path());
    kill_in(dir.path());
    assert_eq!(result.unwrap_err(), "it did not answer in time");
    assert!(took < Duration::from_secs(3), "took {took:?}");
    assert!(ended, "left: {left}");
}

#[test]
fn run_with_password_refuses_two_lines_before_the_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let spawn = sidecar(dir.path(), READS);
    let error = run_with_password(
        &spawn,
        &strings(&["connect"]),
        &Password::new("a\nb"),
        Duration::from_secs(10),
    )
    .unwrap_err();
    assert_eq!(error, "The password must be one line.");
    assert_eq!(argv(dir.path()), None);
}

#[test]
fn the_password_is_cut_from_errors() {
    let dir = tempfile::tempdir().unwrap();
    let spawn = sidecar(
        dir.path(),
        r#"read p
echo "error: bad password $p" >&2
exit 1"#,
    );
    let data = dir.path().join("data");
    let error = create_account(
        &spawn,
        &data,
        "me",
        "me@example.org",
        &Password::new(SECRET),
    )
    .unwrap_err();
    assert_eq!(error, "The command failed.");
    let error = connect_pipeline(
        &spawn,
        "http://h:19072",
        &data,
        "me@example.org",
        &Password::new(SECRET),
    )
    .unwrap_err();
    assert_eq!(error, "The command failed.");

    // Other error texts pass through as the sidecar wrote them.
    let spawn = sidecar(
        dir.path(),
        r#"read p
echo "warning: slow disk" >&2
echo "error: the email is in use by account anna" >&2
exit 1"#,
    );
    let error = create_account(
        &spawn,
        &data,
        "me",
        "me@example.org",
        &Password::new(SECRET),
    )
    .unwrap_err();
    assert_eq!(error, "the email is in use by account anna");
}

#[test]
fn create_account_runs_cloud_account_add() {
    let dir = tempfile::tempdir().unwrap();
    let spawn = sidecar(dir.path(), READS);
    let data = dir.path().join("data");
    create_account(
        &spawn,
        &data,
        "me",
        "me@example.org",
        &Password::new(SECRET),
    )
    .unwrap();
    let data = data.display().to_string();
    assert_eq!(
        argv(dir.path()).unwrap(),
        strings(&[
            "cloud",
            "account",
            "--data",
            &data,
            "add",
            "me",
            "--email",
            "me@example.org",
            "--password-stdin",
        ])
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("stdin")).unwrap(),
        "secret\n"
    );
}

#[test]
fn create_account_refuses_before_the_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let spawn = sidecar(dir.path(), READS);
    let data = dir.path().join("data");
    let two_lines = Password::new("a\nb");
    assert_eq!(
        create_account(&spawn, &data, "me", "me@example.org", &two_lines).unwrap_err(),
        "The password must be one line."
    );
    for slug in ["", "--help", "-x", "Me", "a b", &"a".repeat(65)] {
        assert_eq!(
            create_account(
                &spawn,
                &data,
                slug,
                "me@example.org",
                &Password::new(SECRET)
            )
            .unwrap_err(),
            BAD_SLUG,
            "{slug:?}"
        );
    }
    assert_eq!(argv(dir.path()), None);
}

#[test]
fn an_early_error_is_shown_not_the_broken_pipe() {
    let dir = tempfile::tempdir().unwrap();
    let spawn = sidecar(
        dir.path(),
        r#"echo "error: account me already exists" >&2
exit 1"#,
    );
    // Longer than a pipe holds: the write meets the closed pipe for sure.
    let long = "x".repeat(200_000);
    let error = create_account(
        &spawn,
        &dir.path().join("data"),
        "me",
        "me@example.org",
        &Password::new(long),
    )
    .unwrap_err();
    assert_eq!(error, "account me already exists");
}

fn connect_argv(dir: &Path, url: &str) -> Vec<String> {
    let data = dir.join("data");
    let spawn = sidecar(dir, READS);
    connect_pipeline(&spawn, url, &data, "me@example.org", &Password::new(SECRET)).unwrap();
    assert_eq!(fs::read_to_string(dir.join("stdin")).unwrap(), "secret\n");
    argv(dir).unwrap()
}

#[test]
fn connect_pipeline_runs_connect() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data").display().to_string();
    assert_eq!(
        connect_argv(dir.path(), "http://h:19072/"),
        strings(&[
            "connect",
            "http://h:19072",
            "--data",
            &data,
            "--email",
            "me@example.org",
            "--password-stdin",
            "--name",
            "Notedown for Mac pipeline",
        ])
    );
}

#[test]
fn connect_pipeline_strips_rest_v1() {
    for url in [
        "http://h:19072/rest/v1/",
        "http://h:19072/rest/v1",
        "http://h:19072",
    ] {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(connect_argv(dir.path(), url)[1], "http://h:19072", "{url}");
    }
}

fn config_in(dir: &Path, config: AppConfig) -> ConfigState {
    config::save(dir, &config).unwrap();
    ConfigState::load(dir.to_path_buf())
}

#[test]
fn connect_configured_uses_the_config() {
    let dir = tempfile::tempdir().unwrap();
    let spawn = sidecar(dir.path(), READS);
    let remote = AppConfig {
        mode: Mode::Remote,
        server_url: Some("https://notes.example.org/".to_owned()),
        data_dir: dir.path().join("mac"),
        email: Some("me@example.org".to_owned()),
        ..AppConfig::default()
    };
    let secret = Password::new(SECRET);
    for mode in [Mode::Local, Mode::NotSet] {
        let config = AppConfig {
            mode,
            ..remote.clone()
        };
        assert_eq!(
            connect_configured(&spawn, &config, &secret).unwrap_err(),
            ONLY_REMOTE
        );
    }
    let no_email = AppConfig {
        email: None,
        ..remote.clone()
    };
    assert_eq!(
        connect_configured(&spawn, &no_email, &secret).unwrap_err(),
        LOG_IN_FIRST
    );
    assert_eq!(argv(dir.path()), None);

    connect_configured(&spawn, &remote, &secret).unwrap();
    let argv = argv(dir.path()).unwrap();
    assert_eq!(argv[1], "https://notes.example.org");
    assert_eq!(argv[3], dir.path().join("mac").display().to_string());
    assert_eq!(argv[5], "me@example.org");
}

/// A Local config whose server listens on `port` (by its `server.toml`).
fn local_on(dir: &Path, port: u16) -> ConfigState {
    let data = dir.join("data");
    fs::create_dir_all(&data).unwrap();
    fs::write(
        data.join("server.toml"),
        format!("listen = [\"127.0.0.1:{port}\"]\n"),
    )
    .unwrap();
    config_in(
        dir,
        AppConfig {
            mode: Mode::Local,
            data_dir: data,
            ..AppConfig::default()
        },
    )
}

fn create_now(
    spawn: &SpawnFn,
    state: &ConfigState,
    store: &dyn TokenStore,
) -> Result<ConfigAnswer, String> {
    create_and_log_in(
        spawn,
        &LoginGate::default(),
        state,
        &ServeInfo::default(),
        store,
        &crate::library::idle(),
        "me",
        "me@example.org",
        &Password::new(SECRET),
    )
}

#[test]
fn a_new_account_is_made_and_logged_in() {
    let server = Scripted::start(vec![
        answer(
            "200 OK",
            r#"{"randomCode":"c0de42","timestamp":1763300000000}"#,
        ),
        answer(
            "201 Created",
            r#"{"token":"tok-123","id":"t1","slug":"me","userId":7,"scope":"write"}"#,
        ),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let spawn = sidecar(dir.path(), READS);
    let state = local_on(dir.path(), server.port);
    let store = MemoryStore::new();
    let saved = create_now(&spawn, &state, &store).unwrap();
    assert_eq!(saved.config.account.as_deref(), Some("me"));
    assert_eq!(saved.config.email.as_deref(), Some("me@example.org"));
    assert_eq!(store.get("local|me").unwrap().unwrap().expose(), "tok-123");
    assert_eq!(argv(dir.path()).unwrap()[4], "add");
    assert_eq!(server.next().target, "/rest/v1/login/code");
}

#[test]
fn a_made_account_whose_login_fails_says_log_in() {
    let dir = tempfile::tempdir().unwrap();
    let spawn = sidecar(dir.path(), READS);
    // Port 1: nothing listens there in the tests (macOS lets any process bind a low port,
    // but no test binds this one), so the login is refused at once.
    let state = local_on(dir.path(), 1);
    let error = create_now(&spawn, &state, &MemoryStore::new()).unwrap_err();
    assert_eq!(error, "The account is made. Log in with it.");
    assert!(argv(dir.path()).is_some());
}

#[test]
fn a_failed_create_does_not_log_in() {
    let dir = tempfile::tempdir().unwrap();
    let spawn = sidecar(
        dir.path(),
        r#"echo "error: account me already exists" >&2
exit 1"#,
    );
    let state = local_on(dir.path(), 1);
    let error = create_now(&spawn, &state, &MemoryStore::new()).unwrap_err();
    assert_eq!(error, "account me already exists");
}

#[test]
fn accounts_on_another_machine_are_made_there() {
    let dir = tempfile::tempdir().unwrap();
    let spawn = sidecar(dir.path(), READS);
    for (mode, text) in [
        (Mode::Remote, "Accounts on another machine are made there."),
        (Mode::NotSet, "Choose where the server runs first."),
    ] {
        let state = config_in(
            dir.path(),
            AppConfig {
                mode,
                server_url: Some("http://h:19072".to_owned()),
                ..AppConfig::default()
            },
        );
        let error = create_now(&spawn, &state, &MemoryStore::new()).unwrap_err();
        assert_eq!(error, text, "{mode:?}");
    }
    assert_eq!(argv(dir.path()), None);
}

fn account_file(data: &Path, slug: &str, text: &str) {
    let dir = data.join("accounts").join(slug);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("account.json"), text).unwrap();
}

fn account_json(slug: &str, email: &str, imported: bool) -> String {
    let imported = if imported {
        r#","imported":{"at":"2026-11-17T10:00:00Z","files":12}"#
    } else {
        ""
    };
    format!(
        r#"{{"slug":"{slug}","email":"{email}","user_id":7,"user_name":"{slug}","password_md5":"0000","created":"2026-11-01","locked":false,"failed_logins":0{imported}}}"#
    )
}

fn account(slug: &str, email: &str, imported: bool) -> ExistingAccount {
    ExistingAccount {
        slug: slug.to_owned(),
        email: email.to_owned(),
        imported,
    }
}

#[test]
fn existing_details_lists_accounts_and_the_pipeline_url() {
    let home = tempfile::tempdir().unwrap();
    let server = home.path().join("notedown-data");
    let mac = home.path().join("notedown-mac");
    let empty = home.path().join("empty");
    let missing = home.path().join("missing");
    account_file(
        &server,
        "jordi",
        &account_json("jordi", "j@example.org", true),
    );
    account_file(
        &server,
        "anna",
        &account_json("anna", "a@example.org", false),
    );
    fs::write(server.join("server.toml"), "listen = [\"0.0.0.0:19072\"]\n").unwrap();
    fs::create_dir_all(&mac).unwrap();
    fs::write(
        mac.join("pipeline.toml"),
        "[server]\nurl = \"http://h:19072\"\n",
    )
    .unwrap();
    fs::create_dir_all(&empty).unwrap();

    let found = existing_details(&[server.clone(), mac.clone(), empty, missing]);
    assert_eq!(
        found,
        vec![
            ExistingFolder {
                path: server,
                server: Some(ServerFolder {
                    accounts: vec![
                        account("anna", "a@example.org", false),
                        account("jordi", "j@example.org", true),
                    ],
                }),
                pipeline: None,
            },
            ExistingFolder {
                path: mac,
                server: None,
                pipeline: Some(PipelineFolder {
                    url: Some("http://h:19072".to_owned()),
                }),
            },
        ]
    );
    let json = serde_json::to_value(&found).unwrap();
    assert_eq!(json[0]["server"]["accounts"][1]["imported"], true);
    assert_eq!(json[1]["pipeline"]["url"], "http://h:19072");
    assert!(json[1]["server"].is_null());
}

#[test]
fn existing_details_skips_a_broken_account_file() {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("data");
    account_file(&data, "good", &account_json("good", "g@example.org", false));
    account_file(&data, "broken", "{ not json");
    account_file(&data, "half", r#"{"slug":"half"}"#);
    // A pipeline.toml without a server address still is a pipeline.
    fs::write(data.join("pipeline.toml"), "[pipeline\n").unwrap();
    let found = existing_details(std::slice::from_ref(&data));
    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].server,
        Some(ServerFolder {
            accounts: vec![account("good", "g@example.org", false)],
        })
    );
    assert_eq!(found[0].pipeline, Some(PipelineFolder { url: None }));

    // A server folder without any readable account is still a server.
    let only_broken = home.path().join("only-broken");
    account_file(&only_broken, "broken", "");
    let found = existing_details(&[only_broken]);
    assert_eq!(
        found[0].server,
        Some(ServerFolder {
            accounts: Vec::new()
        })
    );
}

#[test]
fn existing_details_looks_in_every_folder_once() {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("notedown-data");
    let mac = home.path().join("notedown-mac");
    account_file(&data, "me", &account_json("me", "me@example.org", false));
    fs::create_dir_all(&mac).unwrap();
    fs::write(mac.join("pipeline.toml"), "").unwrap();
    // The config's folder is `~/notedown-data` too, once written with a dot in it.
    // A link to it is the same folder too.
    let link = home.path().join("link-to-data");
    std::os::unix::fs::symlink(&data, &link).unwrap();
    let found = existing_details(&[data.clone(), data.join("."), link.clone(), mac.clone()]);
    let paths: Vec<&Path> = found.iter().map(|f| f.path.as_path()).collect();
    assert_eq!(paths, vec![data.as_path(), mac.as_path()]);
    // The link alone is listed as written.
    let found = existing_details(std::slice::from_ref(&link));
    assert_eq!(found[0].path, link);
}

#[test]
fn the_setup_looks_in_the_config_folder_and_the_two_usual_ones() {
    let config = AppConfig {
        data_dir: PathBuf::from("/x/data"),
        ..AppConfig::default()
    };
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    assert_eq!(
        setup_folders(&config),
        vec![
            PathBuf::from("/x/data"),
            home.join("notedown-data"),
            home.join("notedown-mac"),
        ]
    );
}

// The check of the first sync after the import.

fn events_page(events: &str, next: u64, end: u64, more: bool, gap: bool) -> String {
    answer(
        "200 OK",
        &format!(r#"{{"events":[{events}],"next":{next},"end":{end},"more":{more},"gap":{gap}}}"#),
    )
}

fn event(seq: u64, kind: &str, origin: &str) -> String {
    format!(
        r#"{{"seq":{seq},"ts":1,"type":"{kind}","origin":"{origin}","device":"SN1","opId":"o{seq}","path":"/Note/a.note"}}"#
    )
}

fn rest_on(server: &Scripted) -> Rest {
    Rest::new(&server.base(), Some(crate::token::Token::new("t"))).unwrap()
}

#[test]
fn journal_end_reads_the_events_end() {
    let server = Scripted::start(vec![events_page("", 0, 41, false, false)]);
    assert_eq!(journal_end(&rest_on(&server)).unwrap(), 41);
    let request = server.next();
    assert_eq!(request.method, "GET");
    assert_eq!(request.target, "/rest/v1/events?from=0&limit=1");
}

/// A journal line of `cloud journal tail`.
fn line(seq: u64, account: &str, origin: &str, kind: &str) -> String {
    format!(
        r#"{{"seq":{seq},"ts":1,"account":"{account}","origin":"{origin}","device":"SN1","type":"{kind}","path":"/Note/a.note","opId":"o{seq}"}}"#
    )
}

/// A sidecar that prints `lines` as `journal tail` does.
fn tail_sidecar(dir: &Path, lines: &[String]) -> SpawnFn {
    let text = lines.join("\n");
    fs::write(dir.join("tail.jsonl"), format!("{text}\n")).unwrap();
    sidecar(dir, r#"cat "$D/tail.jsonl""#)
}

#[test]
fn sync_check_local_counts_device_deletes_of_the_account() {
    let dir = tempfile::tempdir().unwrap();
    let spawn = tail_sidecar(
        dir.path(),
        &[
            line(11, "me", "device", "file.deleted"),
            line(12, "me", "cli", "file.deleted"),
            line(13, "anna", "device", "file.deleted"),
            line(14, "me", "device", "digest.deleted"),
            line(15, "me", "device", "file.modified"),
            line(16, "me", "device", "folder.deleted"),
            line(17, "me", "device", "file.deleted"),
        ],
    );
    let data = dir.path().join("data");
    let check = sync_check_local(&spawn, &data, "me", 10, 16).unwrap();
    assert_eq!(
        check,
        SyncCheck {
            files: 1,
            folders: 1,
            digests: Some(1),
            cannot_tell: false
        }
    );
    let argv = argv(dir.path()).unwrap();
    assert_eq!(
        argv,
        strings(&[
            "cloud",
            "journal",
            "tail",
            "--data",
            &data.display().to_string(),
            "--from",
            "11"
        ])
    );
}

#[test]
fn the_event_at_the_mark_is_not_counted() {
    let dir = tempfile::tempdir().unwrap();
    // A tail that starts at the mark (as an older CLI might): the mark's own line is the
    // import's, never the first sync's.
    let spawn = tail_sidecar(
        dir.path(),
        &[
            line(10, "me", "device", "file.deleted"),
            line(11, "me", "device", "file.modified"),
        ],
    );
    let check = sync_check_local(&spawn, dir.path(), "me", 10, 11).unwrap();
    assert_eq!(
        check,
        SyncCheck {
            digests: Some(0),
            ..SyncCheck::default()
        }
    );
    // Over REST the same: the page from the mark is not counted.
    let server = Scripted::start(vec![events_page(
        &[
            event(10, "file.deleted", "device"),
            event(11, "file.modified", "device"),
        ]
        .join(","),
        12,
        11,
        false,
        false,
    )]);
    let check = sync_check_remote(&rest_on(&server), 10, 11).unwrap();
    assert_eq!(check, SyncCheck::default());
    assert_eq!(server.next().target, "/rest/v1/events?from=11");
}

#[test]
fn a_missing_line_after_the_mark_cannot_tell() {
    let dir = tempfile::tempdir().unwrap();
    // The journal was cut since the mark: the lines after it are gone.
    let spawn = tail_sidecar(dir.path(), &[line(20, "me", "device", "file.modified")]);
    let check = sync_check_local(&spawn, dir.path(), "me", 10, 20).unwrap();
    assert!(check.cannot_tell);
    // Nothing at all while the end is past the mark: no line can be read.
    let spawn = tail_sidecar(dir.path(), &[]);
    let check = sync_check_local(&spawn, dir.path(), "me", 10, 20).unwrap();
    assert!(check.cannot_tell);
    // Nothing changed since the mark: nothing to tell.
    let check = sync_check_local(&spawn, dir.path(), "me", 10, 10).unwrap();
    assert_eq!(
        check,
        SyncCheck {
            digests: Some(0),
            ..SyncCheck::default()
        }
    );
}

#[test]
fn a_failed_tail_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let spawn = sidecar(
        dir.path(),
        r#"echo "error: data folder /x does not exist" >&2; exit 1"#,
    );
    let error = sync_check_local(&spawn, dir.path(), "me", 10, 20).unwrap_err();
    assert!(error.contains("data folder /x does not exist"), "{error}");
}

#[test]
fn sync_check_remote_pages_events() {
    let server = Scripted::start(vec![
        events_page(
            &[
                event(11, "file.deleted", "device"),
                event(12, "file.deleted", "app"),
            ]
            .join(","),
            13,
            20,
            true,
            false,
        ),
        events_page(
            &[
                event(13, "folder.deleted", "device"),
                event(19, "file.deleted", "device"),
                event(21, "file.deleted", "device"),
            ]
            .join(","),
            22,
            21,
            false,
            false,
        ),
    ]);
    let check = sync_check_remote(&rest_on(&server), 10, 20).unwrap();
    assert_eq!(
        check,
        SyncCheck {
            files: 2,
            folders: 1,
            digests: None,
            cannot_tell: false
        }
    );
    assert_eq!(server.next().target, "/rest/v1/events?from=11");
    assert_eq!(server.next().target, "/rest/v1/events?from=13");
}

#[test]
fn a_gap_cannot_tell() {
    let server = Scripted::start(vec![events_page(
        &event(15, "file.modified", "device"),
        16,
        16,
        false,
        true,
    )]);
    let check = sync_check_remote(&rest_on(&server), 10, 16).unwrap();
    assert!(check.cannot_tell);
    assert_eq!(check.digests, None);
}

// The count of the files a device sync changed.

/// An event of the entry `id`.
fn entry_event(seq: u64, kind: &str, origin: &str, id: u64) -> String {
    format!(
        r#"{{"seq":{seq},"ts":1763290000123,"type":"{kind}","origin":"{origin}","device":"SN1","opId":"o{seq}","id":{id},"path":"/Note/{id}.note"}}"#
    )
}

#[test]
fn the_count_takes_each_file_the_device_changed_once() {
    let server = Scripted::start(vec![
        events_page(
            &[
                // The event at the cursor was counted last time.
                entry_event(10, "file.modified", "device", 9),
                entry_event(11, "file.created", "device", 1),
                entry_event(12, "file.modified", "device", 1),
                entry_event(13, "file.modified", "app", 2),
                entry_event(14, "folder.created", "device", 3),
            ]
            .join(","),
            15,
            20,
            true,
            false,
        ),
        events_page(
            &[
                entry_event(15, "file.moved", "device", 1),
                entry_event(16, "file.deleted", "device", 4),
                entry_event(17, "file.copied", "device", 5),
                entry_event(18, "folder.deleted", "device", 6),
                entry_event(19, "file.modified", "scan", 7),
                entry_event(20, "file.modified", "device", 8),
                // Past the end: the next sync's.
                entry_event(21, "file.modified", "device", 10),
            ]
            .join(","),
            22,
            21,
            false,
            false,
        ),
    ]);
    // Files 1, 4, 5 and 8.
    assert_eq!(
        count_device_files(&rest_on(&server), 10, 20).unwrap(),
        Some(4)
    );
    assert_eq!(server.next().target, "/rest/v1/events?from=11");
    assert_eq!(server.next().target, "/rest/v1/events?from=15");
}

#[test]
fn nothing_after_the_cursor_is_no_file_and_no_call() {
    let server = Scripted::start(vec![]);
    assert_eq!(
        count_device_files(&rest_on(&server), 20, 20).unwrap(),
        Some(0)
    );
    assert!(server.received().is_empty());
}

#[test]
fn a_gap_gives_no_count() {
    let server = Scripted::start(vec![events_page(
        &entry_event(15, "file.modified", "device", 1),
        16,
        16,
        false,
        true,
    )]);
    assert_eq!(count_device_files(&rest_on(&server), 10, 16).unwrap(), None);
}

#[test]
fn more_than_the_page_bound_gives_no_count() {
    // Each page moves on by one and says there is more.
    let pages = (0..=COUNT_PAGES as u64)
        .map(|n| {
            events_page(
                &entry_event(11 + n, "file.modified", "device", n),
                12 + n,
                1_000,
                true,
                false,
            )
        })
        .collect();
    let server = Scripted::start(pages);
    assert_eq!(
        count_device_files(&rest_on(&server), 10, 1_000).unwrap(),
        None
    );
    assert_eq!(server.received().len(), COUNT_PAGES);
    // Within the bound the count is there.
    let pages = (0..COUNT_PAGES as u64)
        .map(|n| {
            events_page(
                &entry_event(11 + n, "file.modified", "device", n),
                12 + n,
                10 + COUNT_PAGES as u64,
                true,
                false,
            )
        })
        .collect();
    let server = Scripted::start(pages);
    assert_eq!(
        count_device_files(&rest_on(&server), 10, 10 + COUNT_PAGES as u64).unwrap(),
        Some(COUNT_PAGES as u64)
    );
}

#[test]
fn a_failed_page_is_the_error() {
    let server = Scripted::start(vec![answer(
        "500 Internal Server Error",
        r#"{"error":"internal"}"#,
    )]);
    assert!(count_device_files(&rest_on(&server), 10, 16).is_err());
}

#[test]
fn the_sync_state_is_the_check_of_the_login() {
    use crate::config::SyncMark;
    let mut config = AppConfig {
        mode: Mode::Local,
        account: Some("me".into()),
        ..AppConfig::default()
    };
    config.setup.sync_mark = Some(SyncMark {
        key: "local|me".into(),
        seq: 4,
        at_ms: 1,
        sync_at_mark: None,
    });
    config.setup.sync_checked = Some(SyncCheck {
        files: 2,
        ..SyncCheck::default()
    });
    config.setup.sync_acknowledged = true;
    let state = sync_state(&config);
    assert_eq!(state.checked.map(|c| c.files), Some(2));
    assert!(state.acknowledged);
    // The mark of another login is not this one's.
    config.account = Some("anna".into());
    assert_eq!(sync_state(&config), SyncState::default());
}
