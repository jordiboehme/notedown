use notedown_cloud::auth::Scope;

const NOTE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/notes/test2.note"
);

#[test]
fn render_command_writes_pngs() {
    let dir = tempfile::tempdir().unwrap();
    let st = std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
        .args(["render", NOTE, "--out"])
        .arg(dir.path())
        .status()
        .unwrap();
    assert!(st.success());
    assert!(dir.path().join("test2-page-001.png").exists());
}

#[cfg(feature = "ocr")]
#[test]
fn convert_reports_missing_model_with_hint() {
    let dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
        .env("NOTEDOWN_MODEL_DIR", dir.path())
        .args(["convert", NOTE, "--out"])
        .arg(dir.path().join("o"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("notedown model pull"));
}

#[cfg(feature = "ocr")]
#[test]
fn convert_updates_an_old_header_without_loading_the_model() {
    use sha2::Digest;
    let dir = tempfile::tempdir().unwrap();
    let note = dir.path().join("Test 2.note");
    std::fs::copy(NOTE, &note).unwrap();
    let sha = const_hex::encode(sha2::Sha256::digest(std::fs::read(&note).unwrap()));
    let body = "\n<a id=\"page-1\"></a>\n## Page 1\n\nOld text\n";
    let old = format!(
        "---\nsource: Test 2.note\nsource_sha256: {sha}\nkind: supernote-note\npages: 1\n\
         model: chandra-ocr-2 Q4_K_M\nconverted_at: \"2026-10-01T08:00:00Z\"\n\
         generator: notedown {}\nkeywords: []\n---\n{body}",
        env!("CARGO_PKG_VERSION")
    );
    std::fs::write(dir.path().join("Test 2.md"), old).unwrap();
    // an empty model folder: loading the model would fail the run
    let models = tempfile::tempdir().unwrap();
    // the usual call: a relative file name, the .md next to it
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
        .env("NOTEDOWN_MODEL_DIR", models.path())
        .current_dir(dir.path())
        .args(["convert", "Test 2.note"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("updated the header of Test 2.note"),
        "{stderr}"
    );
    assert!(stderr.contains("1 header updated"), "{stderr}");
    assert!(!stderr.contains("loading model"), "{stderr}");
    let md = std::fs::read_to_string(dir.path().join("Test 2.md")).unwrap();
    assert!(md.starts_with("---\ntype: note\n"), "{md}");
    assert!(md.ends_with(&format!("\n---\n{body}")), "{md}");
    assert!(md.contains("\nresource: \"Test 2.note\"\n"), "{md}");
    assert!(
        md.contains("\n  - { id: note, resource: \"Test 2.note\", last_modified: "),
        "{md}"
    );
}

#[cfg(feature = "ocr")]
#[test]
fn model_path_prints_cache_dir() {
    let dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
        .env("NOTEDOWN_MODEL_DIR", dir.path())
        .args(["model", "path"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("chandra-ocr-2.Q4_K_M.gguf"));
}

#[cfg(feature = "ocr")]
#[test]
fn model_verify_fails_for_empty_cache() {
    let dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
        .env("NOTEDOWN_MODEL_DIR", dir.path())
        .args(["model", "verify"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
}

fn cloud(data: &std::path::Path, args: &[&str], stdin: Option<&str>) -> std::process::Output {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
        .args(["cloud", "account", "--data"])
        .arg(data)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut pipe = child.stdin.take().unwrap();
    if let Some(text) = stdin {
        pipe.write_all(text.as_bytes()).unwrap();
    }
    drop(pipe);
    child.wait_with_output().unwrap()
}

#[test]
fn cloud_account_remove_of_a_typo_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let typo = dir.path().join("typo");
    let out = cloud(&typo, &["remove", "jordi", "--yes"], None);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("does not exist"));
    assert!(!typo.join("journal").exists());
}

#[test]
fn cloud_account_add_list_passwd_remove() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let out = cloud(
        d,
        &[
            "add",
            "jordi",
            "--email",
            "jordi@example.org",
            "--password-stdin",
        ],
        Some("secret\n"),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let file = d.join("accounts/jordi/account.json");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains("5ebe2294ecd0e0f08eab7690d2a6ee69"));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("5ebe"));

    let out = cloud(
        d,
        &[
            "add",
            "jordi",
            "--email",
            "x@example.org",
            "--password-stdin",
        ],
        Some("pw\n"),
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("already exists"));

    let out = cloud(d, &["list"], None);
    let listing = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(listing.contains("jordi\tjordi@example.org\t"));
    assert!(listing.contains("active"));
    assert!(!listing.contains("5ebe"));

    let out = cloud(d, &["passwd", "jordi", "--password-stdin"], Some("other\n"));
    assert!(out.status.success());
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(!text.contains("5ebe2294"));

    let out = cloud(d, &["remove", "jordi"], None);
    assert!(!out.status.success());
    assert!(file.exists());
    let out = cloud(d, &["remove", "jordi", "--yes"], None);
    assert!(out.status.success());
    assert!(!file.exists());
    assert!(cloud(d, &["list"], None).stdout.is_empty());

    let out = cloud(
        d,
        &[
            "add",
            "Bad Slug",
            "--email",
            "a@example.org",
            "--password-stdin",
        ],
        Some("pw\n"),
    );
    assert!(!out.status.success());
    let out = cloud(
        d,
        &["add", "x", "--email", "a@example.org", "--password-stdin"],
        Some("\n"),
    );
    assert!(!out.status.success());
}

/// Start `notedown serve` on a loopback port and return the startup lines up to the stop hint.
fn serve_startup(data: &std::path::Path, extra: &[&str]) -> String {
    serve_startup_with_stderr(data, extra).0
}

/// The start lines of `notedown serve` on stdout, and what it wrote on stderr until then.
fn serve_startup_with_stderr(data: &std::path::Path, extra: &[&str]) -> (String, String) {
    use std::io::{BufRead, Read};
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
        .args(["serve", "--data"])
        .arg(data)
        .args(extra)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = String::new();
    let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        lines.push_str(&line);
        if line.starts_with("press Ctrl-C") {
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    (lines, err)
}

#[test]
fn serve_says_mcp_runs_over_stdio_when_server_toml_has_mcp() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("server.toml"),
        "listen = [\"127.0.0.1:0\"]\n[mcp]\nenabled = true\n",
    )
    .unwrap();
    let out = serve_startup(dir.path(), &[]);
    assert!(
        out.contains(
            "\nmcp: [mcp] in server.toml is ignored; MCP runs on the pipeline machine over stdio (notedown mcp)\n"
        ),
        "{out}"
    );
    assert!(!out.contains("\nmcp: http"), "{out}");
}

#[test]
fn serve_reads_the_old_push_reconnect_as_detail_and_warns() {
    let dir = tempfile::tempdir().unwrap();
    let toml = dir.path().join("server.toml");
    let text = "listen = [\"127.0.0.1:0\"]\npush = \"reconnect\"\n";
    std::fs::write(&toml, text).unwrap();
    let (out, err) = serve_startup_with_stderr(dir.path(), &[]);
    assert!(out.contains("\npush mode: detail ("), "{out}");
    assert_eq!(err.matches("push = \"reconnect\"").count(), 1, "{err}");
    assert!(
        err.contains(
            "warning: server.toml: push = \"reconnect\" is gone, using \"detail\". Remove the \
             line or set push = \"detail\".\n"
        ),
        "{err}"
    );
    assert_eq!(std::fs::read_to_string(&toml).unwrap(), text);
}

#[test]
fn serve_refuse_sync_flag_counts_for_one_run_and_keeps_server_toml() {
    let dir = tempfile::tempdir().unwrap();
    let toml = dir.path().join("server.toml");
    std::fs::write(&toml, "listen = [\"127.0.0.1:0\"]\n").unwrap();
    let out = serve_startup(dir.path(), &["--refuse-sync"]);
    assert!(
        out.contains("sync mode: refuse (capture only, no files are exchanged)"),
        "{out}"
    );
    assert_eq!(
        std::fs::read_to_string(&toml).unwrap(),
        "listen = [\"127.0.0.1:0\"]\n"
    );
    assert!(out.starts_with("notedown cloud server\n"), "{out}");
    let out = serve_startup(dir.path(), &[]);
    assert!(out.starts_with("notedown cloud server\n"), "{out}");
    assert!(!out.contains("capture server"), "{out}");
    assert!(out.contains("sync mode: accept (protect: on)"), "{out}");
    assert!(
        out.contains(
            "\npush mode: detail (each changed file and folder, after the device's next heartbeat)\n"
        ),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "\nrender: {}\n",
            dir.path().join("share").display()
        )),
        "{out}"
    );
    assert!(out.contains("\nwatch: on\n"), "{out}");
    assert!(
        out.contains("\nscan of the data folder: every 300 s, at sync start and before listings\n"),
        "{out}"
    );
    assert!(
        out.contains("\nscan hold: when the account would be left without files\n"),
        "{out}"
    );
}

#[test]
fn serve_names_the_scan_settings() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("server.toml"),
        "listen = [\"127.0.0.1:0\"]\nscan_interval = 0\nscan_min_files = 5\n",
    )
    .unwrap();
    let out = serve_startup(dir.path(), &[]);
    assert!(
        out.contains("\nscan of the data folder: at sync start and before listings (no timer)\n"),
        "{out}"
    );
    assert!(
        out.contains("\nscan hold: when fewer than 5 files would be left\n"),
        "{out}"
    );
}

/// `notedown cloud journal <action> --data <dir> <rest>`, the documented argument order.
fn journal(data: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
        .args(["cloud", "journal", args[0], "--data"])
        .arg(data)
        .args(&args[1..])
        .output()
        .unwrap()
}

fn lines(out: &std::process::Output) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// An account with one file and an unfinished commit (a crash after the folder was created,
/// before its event was journaled).
fn journal_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let out = cloud(
        d,
        &[
            "add",
            "jordi",
            "--email",
            "jordi@example.org",
            "--password-stdin",
        ],
        Some("secret\n"),
    );
    assert!(out.status.success());
    let files = d.join("accounts/jordi/files");
    std::fs::create_dir_all(files.join("INBOX/New")).unwrap();
    std::fs::write(files.join("INBOX/a.pdf"), b"pdf").unwrap();
    std::fs::write(
        d.join("accounts/jordi/pending.json"),
        r#"{"opId":"op-x","origin":"device","device":"SN1","op":{"op":"createFolder","id":5000,"path":"INBOX/New"},"events":[{"type":"folder.created","id":5000,"path":"INBOX/New"}]}"#,
    )
    .unwrap();
    dir
}

#[test]
fn cloud_journal_recovers_snapshots_and_tails() {
    let dir = journal_fixture();
    let d = dir.path();
    // every command recovers first: the missing event is journaled once
    let out = journal(d, &["tail"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("account jordi: finished the change op-x")
    );
    let ev = lines(&out);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0]["type"], "folder.created");
    assert_eq!(ev[0]["origin"], "repair");
    assert_eq!(ev[0]["opId"], "op-x");
    assert!(!d.join("accounts/jordi/pending.json").exists());

    // The folder and the file made by hand are journaled by a scan first (seq 2 to 4), so the
    // snapshot gives nothing an id without its event.
    let out = journal(d, &["snapshot", "--account", "jordi"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "journaled 3 events for account jordi (seq 5 to 7)\n"
    );
    let ev = lines(&journal(d, &["tail"]));
    let got: Vec<(&str, &str)> = ev
        .iter()
        .map(|e| (e["type"].as_str().unwrap(), e["origin"].as_str().unwrap()))
        .collect();
    assert_eq!(
        got,
        [
            ("folder.created", "repair"),
            ("folder.created", "scan"),
            ("folder.created", "scan"),
            ("file.created", "scan"),
            ("folder.created", "snapshot"),
            ("folder.created", "snapshot"),
            ("file.created", "snapshot"),
        ]
    );
    assert_eq!(ev[6]["path"], "INBOX/a.pdf");
    assert_eq!(ev[6]["kind"], "document");
    let ev = lines(&journal(d, &["tail", "--from", "6"]));
    assert_eq!(
        ev.iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [6, 7]
    );
    let out = journal(d, &["snapshot", "--account", "ghost"]);
    assert!(!out.status.success());
}

/// A snapshot while a scan holds the account refuses and names the way out.
#[test]
fn cloud_journal_snapshot_refuses_while_the_account_is_held() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let files = scan_account(d, "jordi");
    std::fs::write(files.join("INBOX/a.pdf"), "one").unwrap();
    assert!(scan(d, &["--account", "jordi"]).status.success());
    std::fs::remove_file(files.join("INBOX/a.pdf")).unwrap();
    let before = lines(&journal(d, &["tail"])).len();
    let out = journal(d, &["snapshot", "--account", "jordi"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("account jordi is on hold"), "{err}");
    assert!(
        err.contains(&format!(
            "run: notedown cloud scan --data {} --account jordi --accept",
            d.display()
        )),
        "{err}"
    );
    assert!(d.join("accounts/jordi/scan-hold.json").is_file());
    assert_eq!(lines(&journal(d, &["tail"])).len(), before);
}

/// `put` refuses the same way while the account is held.
#[test]
fn cloud_put_refuses_while_the_account_is_held() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let files = scan_account(d, "jordi");
    std::fs::write(files.join("INBOX/a.pdf"), "one").unwrap();
    assert!(scan(d, &["--account", "jordi"]).status.success());
    std::fs::remove_file(files.join("INBOX/a.pdf")).unwrap();
    assert_eq!(scan(d, &["--account", "jordi"]).status.code(), Some(1));
    let src = d.join("b.pdf");
    std::fs::write(&src, "two").unwrap();
    let out = notedown(&[
        "cloud",
        "put",
        "--data",
        d.to_str().unwrap(),
        "--account",
        "jordi",
        "--to",
        "INBOX",
        src.to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("account jordi is on hold"), "{err}");
    assert!(err.contains("--data"), "{err}");
    assert!(!files.join("INBOX/b.pdf").exists());
}

#[cfg(unix)]
#[test]
fn cloud_journal_tail_follow_prints_new_events_and_stops_on_ctrl_c() {
    use std::io::BufRead;
    let dir = journal_fixture();
    let d = dir.path();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
        .args(["cloud", "journal", "tail", "--data"])
        .arg(d)
        .arg("--follow")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let _ = tx.send(line.unwrap());
        }
    });
    let wait = std::time::Duration::from_secs(10);
    let first = rx.recv_timeout(wait).unwrap();
    assert!(first.contains("\"seq\":1"), "{first}");
    // another process appends; the follower prints it
    let out = journal(d, &["snapshot", "--account", "jordi"]);
    assert!(out.status.success());
    for seq in 2..=4 {
        let line = rx.recv_timeout(wait).unwrap();
        assert!(line.contains(&format!("\"seq\":{seq},")), "{line}");
    }
    let st = std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(st.success());
    let status = child.wait().unwrap();
    assert!(status.success(), "{status:?}");
}

fn notedown(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
        .args(args)
        .output()
        .unwrap()
}

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn cloud_import_dry_run_import_and_put() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let out = cloud(
        d,
        &[
            "add",
            "jordi",
            "--email",
            "jordi@example.org",
            "--password-stdin",
        ],
        Some("secret\n"),
    );
    assert!(out.status.success());
    let backup = d.join("backup");
    std::fs::create_dir_all(backup.join("Note/Work")).unwrap();
    std::fs::write(backup.join("Note/Work/a.note"), b"note").unwrap();
    std::fs::create_dir_all(backup.join("Document")).unwrap();
    std::fs::write(backup.join("Document/b.pdf"), b"pdf").unwrap();
    for f in ["MyStyle", "EXPORT", "SCREENSHOT", "INBOX", "REMOVABLE0"] {
        std::fs::create_dir_all(backup.join(f)).unwrap();
    }
    let data = d.to_str().unwrap();
    let b = backup.to_str().unwrap();

    let out = notedown(&[
        "cloud",
        "import",
        b,
        "--data",
        data,
        "--account",
        "jordi",
        "--dry-run",
        "--no-report",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        stdout(&out),
        "dry run, nothing is written\n\
         Document -> DOCUMENT/Document: 1 file, 0 folders, 0.0 MB\n\
         EXPORT -> EXPORT: 0 files, 0 folders, 0.0 MB\n\
         INBOX -> INBOX: 0 files, 0 folders, 0.0 MB\n\
         MyStyle -> NOTE/MyStyle: 0 files, 0 folders, 0.0 MB\n\
         Note -> NOTE/Note: 1 file, 1 folder, 0.0 MB\n\
         SCREENSHOT -> SCREENSHOT: 0 files, 0 folders, 0.0 MB\n\
         skipped REMOVABLE0 (SD card)\n\
         account jordi holds 0 files now\n\
         would import 2 files (0.0 MB)\n"
    );
    assert!(!d.join("accounts/jordi/files").exists());
    assert!(!d.join("journal").exists());

    let refused = notedown(&["cloud", "import", b, "--data", data, "--account", "jordi"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--no-report"));
    let out = notedown(&[
        "cloud",
        "import",
        b,
        "--data",
        data,
        "--account",
        "jordi",
        "--no-report",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout(&out).ends_with("imported 2 files (0.0 MB), protect gate open for jordi\n"),
        "{}",
        stdout(&out)
    );
    let again = notedown(&[
        "cloud",
        "import",
        b,
        "--data",
        data,
        "--account",
        "jordi",
        "--no-report",
    ]);
    assert_eq!(again.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&again.stderr).contains("--replace"));

    let src = d.join("c.pdf");
    std::fs::write(&src, b"c").unwrap();
    let s = src.to_str().unwrap();
    let out = notedown(&[
        "cloud",
        "put",
        s,
        "--data",
        data,
        "--account",
        "jordi",
        "--to",
        "INBOX",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    assert!(text.starts_with("INBOX/c.pdf id "), "{text}");
    assert!(text.ends_with(" (created)\n"), "{text}");
    let out = notedown(&[
        "cloud",
        "put",
        s,
        "--data",
        data,
        "--account",
        "jordi",
        "--to",
        "Other",
    ]);
    assert_eq!(out.status.code(), Some(1));
    let ev = lines(&journal(d, &["tail"]));
    let last = ev.last().unwrap();
    assert_eq!(last["type"], "file.created");
    assert_eq!(last["path"], "INBOX/c.pdf");
    assert_eq!(last["origin"], "cli");
    // no em-dash in what people read
    for o in [&out, &again] {
        assert!(!String::from_utf8_lossy(&o.stderr).contains('\u{2014}'));
    }
}

#[test]
fn cloud_backup_device_exits_non_zero_when_the_device_does_not_answer() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("nomad");
    let out = notedown(&[
        "cloud",
        "backup-device",
        &format!("http://127.0.0.1:{port}"),
        target.to_str().unwrap(),
        "--utc-offset",
        "+02:00",
    ]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(target.join("backup.json")).unwrap()).unwrap();
    assert_eq!(report["errors"].as_array().unwrap().len(), 1);
    assert!(stdout(&out).contains("with 1 error"), "{}", stdout(&out));
}

#[test]
fn cloud_commands_say_when_they_wait_for_a_store_lock() {
    use std::io::BufRead;
    let dir = tempfile::tempdir().unwrap();
    let add = cloud(
        dir.path(),
        &[
            "add",
            "jordi",
            "--email",
            "j@example.org",
            "--password-stdin",
        ],
        Some("secret\n"),
    );
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );
    let account = dir.path().join("accounts/jordi");
    std::fs::create_dir_all(account.join("files")).unwrap();
    // Another process (an import, say) holds the store lock.
    let held = notedown_cloud::store::take_store_lock(&account, None).unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
        .args(["cloud", "account", "--data"])
        .arg(dir.path())
        .arg("list")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut err = std::io::BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    err.read_line(&mut line).unwrap();
    assert_eq!(
        line.trim_end(),
        "waiting for the store lock of account jordi"
    );
    // It waits until the lock is free, then finishes normally.
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(child.try_wait().unwrap().is_none(), "it did not wait");
    drop(held);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("jordi"));
}

#[test]
fn cloud_import_app_data_dry_run_import_and_refusals() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let data = d.to_str().unwrap();
    assert!(
        cloud(
            d,
            &[
                "add",
                "jordi",
                "--email",
                "jordi@example.org",
                "--password-stdin"
            ],
            Some("secret\n"),
        )
        .status
        .success()
    );
    let snbak = d.join("device.snbak");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&snbak).unwrap());
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("backup/DIGEST/knowledge.json", options)
        .unwrap();
    zip.write_all(
        br#"[{"serviceId": 42, "content": "an excerpt", "sourcePath": "Note/x.note",
              "sourceType": 2, "commentHandwriteName": "", "metadata": "{}",
              "knowledgeBaseUniqueAttribute": "g9"},
             {"serviceId": 0, "content": "never synced", "sourcePath": "Note/y.note"}]"#,
    )
    .unwrap();
    zip.finish().unwrap();
    let s = snbak.to_str().unwrap();
    let args = |extra: &[&'static str]| -> Vec<String> {
        let mut v: Vec<String> = [
            "cloud",
            "import-app-data",
            s,
            "--data",
            data,
            "--account",
            "jordi",
        ]
        .map(String::from)
        .to_vec();
        v.extend(extra.iter().map(|e| e.to_string()));
        v
    };
    let run = |v: Vec<String>| notedown(&v.iter().map(String::as_str).collect::<Vec<_>>());

    // The device files come first.
    let out = run(args(&["--dry-run"]));
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("import the device files first"));

    let backup = d.join("backup");
    std::fs::create_dir_all(backup.join("Note")).unwrap();
    std::fs::write(backup.join("Note/a.note"), b"note").unwrap();
    for f in ["Document", "MyStyle", "EXPORT", "SCREENSHOT", "INBOX"] {
        std::fs::create_dir_all(backup.join(f)).unwrap();
    }
    let out = notedown(&[
        "cloud",
        "import",
        backup.to_str().unwrap(),
        "--data",
        data,
        "--account",
        "jordi",
        "--no-report",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = run(args(&["--dry-run"]));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        stdout(&out),
        "dry run, nothing is written\n\
         42  Note/x.note  an excerpt\n\
         no cloud id yet, left to the device: Note/y.note  never synced\n\
         its group is not imported: 42  Note/x.note  group g9\n\
         1 record left to the device (the device adds it itself at its first sync)\n\
         would import 1 digest, 0 groups, 0 handwritten comments into account jordi\n"
    );
    assert!(!d.join("accounts/jordi/digests").exists());

    let out = run(args(&[]));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        stdout(&out),
        "no cloud id yet, left to the device: Note/y.note  never synced\n\
         its group is not imported: 42  Note/x.note  group g9\n\
         1 record left to the device (the device adds it itself at its first sync)\n\
         imported 1 digest, 0 groups, 0 handwritten comments into account jordi\n"
    );
    assert!(d.join("accounts/jordi/digests/items/42.json").is_file());
    let again = run(args(&[]));
    assert_eq!(again.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&again.stderr).contains("--replace"));

    // --empty and a file are mutually exclusive; one of them is required.
    let both = run(args(&["--empty"]));
    assert_eq!(both.status.code(), Some(2));
    let none = notedown(&[
        "cloud",
        "import-app-data",
        "--data",
        data,
        "--account",
        "jordi",
    ]);
    assert_eq!(none.status.code(), Some(2));
    let out = notedown(&[
        "cloud",
        "import-app-data",
        "--empty",
        "--replace",
        "--data",
        data,
        "--account",
        "jordi",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout(&out)
            .ends_with("imported 0 digests, 0 groups, 0 handwritten comments into account jordi\n"),
        "{}",
        stdout(&out)
    );
}

/// Every file below `dir` with its bytes (the store lock file left out).
fn tree(dir: &std::path::Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    fn walk(
        root: &std::path::Path,
        dir: &std::path::Path,
        out: &mut std::collections::BTreeMap<String, Vec<u8>>,
    ) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            if p.is_dir() {
                out.insert(format!("{rel}/"), Vec::new());
                walk(root, &p, out);
            } else if !rel.ends_with(".store.lock") {
                out.insert(rel, std::fs::read(&p).unwrap());
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

#[test]
fn cloud_recycle_prune_dry_run_then_real_run() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let out = cloud(
        d,
        &[
            "add",
            "jordi",
            "--email",
            "jordi@example.org",
            "--password-stdin",
        ],
        Some("secret\n"),
    );
    assert!(out.status.success());
    // Only the newest old version stays: the result doesn't depend on the clock.
    std::fs::write(
        d.join("server.toml"),
        "recycle_keep_latest = 1\nrecycle_keep_daily = 0\nrecycle_keep_monthly = 0\n",
    )
    .unwrap();
    let backup = d.join("backup");
    for f in [
        "Note",
        "Document",
        "MyStyle",
        "EXPORT",
        "SCREENSHOT",
        "INBOX",
    ] {
        std::fs::create_dir_all(backup.join(f)).unwrap();
    }
    std::fs::write(backup.join("Note/a.note"), b"note").unwrap();
    let data = d.to_str().unwrap();
    let b = backup.to_str().unwrap();
    let out = notedown(&[
        "cloud",
        "import",
        b,
        "--data",
        data,
        "--account",
        "jordi",
        "--no-report",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let src = d.join("c.pdf");
    let s = src.to_str().unwrap();
    for bytes in [&b"one"[..], b"two", b"three"] {
        std::fs::write(&src, bytes).unwrap();
        let out = notedown(&[
            "cloud",
            "put",
            s,
            "--data",
            data,
            "--account",
            "jordi",
            "--to",
            "INBOX",
        ]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let prune = |extra: &[&str]| {
        let mut args = vec![
            "cloud",
            "recycle",
            "prune",
            "--data",
            data,
            "--account",
            "jordi",
        ];
        args.extend_from_slice(extra);
        notedown(&args)
    };

    // The two old versions, oldest first, and a line that can't be read.
    let recycle = d.join("accounts/jordi/recycle");
    let index = std::fs::read_to_string(recycle.join("index.jsonl")).unwrap();
    let slots: Vec<std::path::PathBuf> = index
        .lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            recycle.join(format!(
                "{}/{}",
                v["deletedAt"],
                v["path"].as_str().unwrap()
            ))
        })
        .collect();
    assert_eq!(slots.len(), 2);
    std::fs::write(recycle.join("index.jsonl"), format!("{index}{{not json\n")).unwrap();

    let before = tree(d);
    let out = prune(&["--dry-run"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    let rows: Vec<&str> = text.lines().collect();
    assert_eq!(rows.len(), 3, "{text}");
    assert!(rows[0].starts_with("would remove INBOX/c.pdf  "), "{text}");
    assert!(rows[0].ends_with("  overwritten"), "{text}");
    assert_eq!(rows[1], "would remove 1 old version (0.0 MB), kept 1");
    assert_eq!(rows[2], "kept 1 index line that can't be read");
    assert_eq!(tree(d), before);

    let out = prune(&[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    assert!(text.starts_with("removed INBOX/c.pdf  "), "{text}");
    assert!(
        text.ends_with(
            "\nremoved 1 old version (0.0 MB), kept 1\nkept 1 index line that can't be read\n"
        ),
        "{text}"
    );
    assert!(!slots[0].exists());
    assert!(slots[1].is_file());
    assert!(!text.contains('\u{2014}'));
    let ev = lines(&journal(d, &["tail"]));
    let last = ev.last().unwrap();
    assert_eq!(last["type"], "recycle.pruned");
    assert_eq!(last["path"], "INBOX/c.pdf");
    assert_eq!(last["origin"], "cli");

    let out = notedown(&[
        "cloud",
        "recycle",
        "prune",
        "--data",
        data,
        "--account",
        "nobody",
    ]);
    assert_eq!(out.status.code(), Some(1));
}

#[cfg(unix)]
#[test]
fn cloud_recycle_prune_without_server_toml_creates_nothing_and_reports_failures() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let out = cloud(
        d,
        &[
            "add",
            "jordi",
            "--email",
            "jordi@example.org",
            "--password-stdin",
        ],
        Some("secret\n"),
    );
    assert!(out.status.success());
    // Two deleted files from 1970: gone by the default retention, whatever the clock says.
    let recycle = d.join("accounts/jordi/recycle");
    let mut index = String::new();
    for (id, ms, name) in [(900_000, 1000, "a.pdf"), (900_001, 2000, "b.pdf")] {
        let slot = recycle.join(format!("{ms}/INBOX/{name}"));
        std::fs::create_dir_all(slot.parent().unwrap()).unwrap();
        std::fs::write(&slot, b"x").unwrap();
        index.push_str(&format!(
            "{{\"id\":{id},\"path\":\"INBOX/{name}\",\"md5\":\"m\",\"deletedAt\":{ms},\"reason\":\"deleted\"}}\n"
        ));
    }
    std::fs::write(recycle.join("index.jsonl"), index).unwrap();
    // A used account has its files folder (the store lock would create it otherwise).
    std::fs::create_dir_all(d.join("accounts/jordi/files")).unwrap();
    let data = d.to_str().unwrap();
    let prune = |extra: &[&str]| {
        let mut args = vec![
            "cloud",
            "recycle",
            "prune",
            "--data",
            data,
            "--account",
            "jordi",
        ];
        args.extend_from_slice(extra);
        notedown(&args)
    };

    let before = tree(d);
    let out = prune(&["--dry-run"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout(&out).ends_with("would remove 2 old versions (0.0 MB), kept 0\n"),
        "{}",
        stdout(&out)
    );
    assert_eq!(tree(d), before);
    assert!(!d.join("server.toml").exists());
    assert!(!d.join("journal").exists());

    // A line with a symlink in its slot path: a dry run reports it and still exits with 0.
    let outside = d.join("outside");
    std::fs::create_dir_all(outside.join("INBOX")).unwrap();
    std::fs::write(outside.join("INBOX/c.pdf"), b"keep").unwrap();
    std::os::unix::fs::symlink(&outside, recycle.join("3000")).unwrap();
    let linked = "{\"id\":900002,\"path\":\"INBOX/c.pdf\",\"md5\":\"m\",\"deletedAt\":3000,\"reason\":\"deleted\"}\n";
    let mut index = std::fs::read_to_string(recycle.join("index.jsonl")).unwrap();
    index.push_str(linked);
    std::fs::write(recycle.join("index.jsonl"), index).unwrap();
    let out = prune(&["--dry-run"]);
    assert_eq!(out.status.code(), Some(0));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("recycle/3000/INBOX/c.pdf: a part of its path is a symlink"),
        "{err}"
    );

    // a.pdf can't be removed: b.pdf still goes, a.pdf is listed as an error only and the run
    // exits with 1
    let stuck = recycle.join("1000/INBOX");
    std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o555)).unwrap();
    let out = prune(&[]);
    std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(text.starts_with("removed INBOX/b.pdf  "), "{text}");
    assert!(!text.contains("a.pdf"), "{text}");
    assert!(
        text.ends_with("removed 1 old version (0.0 MB), kept 1\n"),
        "{text}"
    );
    assert!(outside.join("INBOX/c.pdf").is_file());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("recycle/1000/INBOX/a.pdf: can't remove it: "),
        "{err}"
    );
    assert!(!err.contains('\u{2014}'));
    assert!(stuck.join("a.pdf").is_file());
    assert!(!recycle.join("2000").exists());
    assert!(!d.join("server.toml").exists());
}

/// An account `name` with the six native folders and nothing else.
fn scan_account(d: &std::path::Path, name: &str) -> std::path::PathBuf {
    let out = cloud(
        d,
        &[
            "add",
            name,
            "--email",
            &format!("{name}@example.org"),
            "--password-stdin",
        ],
        Some("secret\n"),
    );
    assert!(out.status.success());
    let files = d.join("accounts").join(name).join("files");
    for f in [
        "Note",
        "Document",
        "MyStyle",
        "EXPORT",
        "SCREENSHOT",
        "INBOX",
    ] {
        std::fs::create_dir_all(files.join(f)).unwrap();
    }
    files
}

fn scan(data: &std::path::Path, extra: &[&str]) -> std::process::Output {
    let mut args = vec!["cloud", "scan", "--data", data.to_str().unwrap()];
    args.extend_from_slice(extra);
    notedown(&args)
}

#[test]
fn cloud_scan_dry_run_writes_nothing_then_a_scan_counts_the_changes() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let files = scan_account(d, "jordi");
    for (n, body) in [("a", "one"), ("b", "two"), ("c", "three"), ("d", "four")] {
        std::fs::write(files.join(format!("INBOX/{n}.pdf")), body).unwrap();
    }
    let before = tree(d);
    let out = scan(d, &["--account", "jordi", "--dry-run"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    assert!(text.contains("would add INBOX/a.pdf\n"), "{text}");
    assert!(
        text.contains("account jordi: would journal 10 new\n"),
        "{text}"
    );
    assert_eq!(tree(d), before);
    assert!(!d.join("server.toml").exists());
    assert!(!d.join("journal").exists());

    let out = scan(d, &["--account", "jordi"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout(&out), "account jordi: journaled 10 new\n");
    assert!(!d.join("server.toml").exists());
    let ev = lines(&journal(d, &["tail"]));
    assert!(
        ev.iter()
            .any(|e| e["type"] == "file.created" && e["origin"] == "scan")
    );

    // Nothing changed since.
    let out = scan(d, &["--account", "jordi"]);
    assert_eq!(stdout(&out), "account jordi: no changes\n");

    let inbox = files.join("INBOX");
    for (n, body) in [("e", "five"), ("f", "six"), ("g", "seven")] {
        std::fs::write(inbox.join(format!("{n}.pdf")), body).unwrap();
    }
    std::fs::write(inbox.join("a.pdf"), "one, edited").unwrap();
    std::fs::rename(inbox.join("b.pdf"), inbox.join("B2.pdf")).unwrap();
    std::fs::remove_file(inbox.join("c.pdf")).unwrap();
    std::fs::remove_file(inbox.join("d.pdf")).unwrap();
    let out = scan(d, &["--dry-run"]);
    let text = stdout(&out);
    assert!(text.contains("would update INBOX/a.pdf\n"), "{text}");
    assert!(
        text.contains("would move INBOX/b.pdf to INBOX/B2.pdf\n"),
        "{text}"
    );
    assert!(text.contains("would delete INBOX/c.pdf\n"), "{text}");
    let out = scan(d, &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        stdout(&out),
        "account jordi: journaled 3 new, 1 changed, 1 moved, 2 deleted\n"
    );
    assert!(!stdout(&out).contains('\u{2014}'));
}

/// A dry run after real scans writes nothing either: the registry log of those scans stays as
/// it is.
#[test]
fn cloud_scan_dry_run_after_a_scan_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let files = scan_account(d, "jordi");
    std::fs::write(files.join("INBOX/a.pdf"), "one").unwrap();
    assert!(scan(d, &["--account", "jordi"]).status.success());
    std::fs::write(files.join("INBOX/b.pdf"), "two").unwrap();
    assert!(d.join("accounts/jordi/ids.jsonl").is_file());
    let before = tree(d);
    let out = scan(d, &["--account", "jordi", "--dry-run"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout(&out).contains("would add INBOX/b.pdf"),
        "{}",
        stdout(&out)
    );
    assert_eq!(tree(d), before);
}

/// A dry run writes nothing, so it can't finish an unfinished change: it refuses.
#[test]
fn cloud_scan_dry_run_refuses_an_unfinished_change() {
    let dir = journal_fixture();
    let d = dir.path();
    let before = tree(d);
    let out = scan(d, &["--account", "jordi", "--dry-run"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("account jordi has an unfinished change"),
        "{err}"
    );
    assert!(err.contains("without --dry-run"), "{err}");
    assert_eq!(tree(d), before);
    assert!(d.join("accounts/jordi/pending.json").is_file());
}

#[test]
fn cloud_scan_holds_on_an_empty_folder_until_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let files = scan_account(d, "jordi");
    std::fs::write(files.join("INBOX/a.pdf"), "one").unwrap();
    std::fs::write(files.join("INBOX/b.pdf"), "two").unwrap();
    assert!(scan(d, &["--account", "jordi"]).status.success());
    std::fs::remove_file(files.join("INBOX/a.pdf")).unwrap();
    std::fs::remove_file(files.join("INBOX/b.pdf")).unwrap();

    let out = scan(d, &["--account", "jordi"]);
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(text.contains("would delete 2 files"), "{text}");
    assert!(text.contains("only 0 would be left"), "{text}");
    // The command names the data folder, which it needs, so it can be copied as it is.
    assert!(
        text.contains(&format!(
            "run: notedown cloud scan --data {} --account jordi --accept",
            d.display()
        )),
        "{text}"
    );
    assert!(!text.contains('\u{2014}'));
    assert!(d.join("accounts/jordi/scan-hold.json").is_file());

    // A dry run on a hold also exits with 1 and keeps the marker.
    let out = scan(d, &["--account", "jordi", "--dry-run"]);
    assert_eq!(out.status.code(), Some(1));

    let out = scan(d, &["--account", "jordi", "--accept"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    assert!(text.contains("journaled 2 deleted"), "{text}");
    assert!(text.contains("the hold is gone"), "{text}");
    assert!(!d.join("accounts/jordi/scan-hold.json").exists());
}

#[test]
fn cloud_scan_min_files_comes_from_server_toml() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let files = scan_account(d, "jordi");
    for n in ["a", "b", "c"] {
        std::fs::write(files.join(format!("INBOX/{n}.pdf")), n).unwrap();
    }
    assert!(scan(d, &[]).status.success());
    std::fs::remove_file(files.join("INBOX/a.pdf")).unwrap();
    std::fs::remove_file(files.join("INBOX/b.pdf")).unwrap();
    std::fs::write(d.join("server.toml"), "scan_min_files = 2\n").unwrap();
    let out = scan(d, &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout(&out).contains("only 1 would be left"),
        "{}",
        stdout(&out)
    );
    std::fs::write(d.join("server.toml"), "scan_min_files = 0\n").unwrap();
    let out = scan(d, &[]);
    assert!(out.status.success());
    assert!(
        stdout(&out).contains("journaled 2 deleted"),
        "{}",
        stdout(&out)
    );
    assert!(
        stdout(&out).contains("the hold is gone"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn cloud_scan_unknown_account_fails_and_every_account_runs_in_turn() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let out = scan(d, &["--account", "nobody"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("nobody"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let files = scan_account(d, "anna");
    std::fs::write(files.join("INBOX/a.pdf"), "one").unwrap();
    let broken = scan_account(d, "broken");
    std::fs::remove_dir_all(&broken).unwrap();
    std::fs::write(&broken, "not a folder").unwrap();
    let files_b = scan_account(d, "zoe");
    std::fs::write(files_b.join("INBOX/z.pdf"), "zed").unwrap();
    let out = scan(d, &[]);
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(text.contains("account anna: journaled 7 new\n"), "{text}");
    assert!(text.contains("account zoe: journaled 7 new\n"), "{text}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("account broken:"), "{err}");
}

#[test]
fn cloud_scan_hold_counts_files_not_folders_and_a_dry_run_only_would_hold() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let files = scan_account(d, "jordi");
    std::fs::create_dir_all(files.join("INBOX/sub")).unwrap();
    std::fs::write(files.join("INBOX/sub/a.pdf"), "one").unwrap();
    std::fs::write(files.join("INBOX/sub/b.pdf"), "two").unwrap();
    assert!(scan(d, &[]).status.success());
    std::fs::remove_dir_all(files.join("INBOX/sub")).unwrap();

    let out = scan(d, &["--dry-run"]);
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(
        text.contains("would delete 2 files, and only 0 would be left"),
        "{text}"
    );
    assert!(text.contains("Device sync would be put on hold."), "{text}");
    assert!(!text.contains("Device sync is on hold."), "{text}");
    assert!(!d.join("accounts/jordi/scan-hold.json").exists());

    let out = scan(d, &[]);
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(
        text.contains("would delete 2 files, and only 0 would be left"),
        "{text}"
    );
    assert!(text.contains("Device sync is on hold."), "{text}");
}

#[test]
fn render_pdf_writes_one_pdf_and_no_pngs() {
    let dir = tempfile::tempdir().unwrap();
    let note = std::path::PathBuf::from(NOTE).with_file_name("test1.note");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
        .args(["render", "--pdf", "--keep-template", "--out"])
        .arg(dir.path())
        .arg(&note)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pdf = dir.path().join("test1.pdf");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        pdf.display().to_string()
    );
    assert!(std::fs::read(&pdf).unwrap().starts_with(b"%PDF-"));
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");
}

/// A data folder with account `acc` and test1 at `NOTE/Note/a.note`.
fn render_fixture() -> tempfile::TempDir {
    let data = tempfile::tempdir().unwrap();
    let out = cloud(
        data.path(),
        &[
            "add",
            "acc",
            "--email",
            "acc@example.org",
            "--password-stdin",
        ],
        Some("secret\n"),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let note = data.path().join("accounts/acc/files/NOTE/Note/a.note");
    std::fs::create_dir_all(note.parent().unwrap()).unwrap();
    std::fs::copy(NOTE.replace("test2", "test1"), &note).unwrap();
    // The render pass reads only notes the store knows: a scan registers the hand-placed one.
    let scan = notedown(&[
        "cloud",
        "scan",
        "--data",
        data.path().to_str().unwrap(),
        "--account",
        "acc",
    ]);
    assert!(
        scan.status.success(),
        "{}",
        String::from_utf8_lossy(&scan.stderr)
    );
    data
}

#[test]
fn cloud_render_writes_the_pdfs_of_an_account() {
    let data = render_fixture();
    let render = tempfile::tempdir().unwrap();
    std::fs::write(
        data.path().join("server.toml"),
        format!("render_dir = {:?}\n", render.path().display().to_string()),
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
        .args(["cloud", "render", "--data"])
        .arg(data.path())
        .args(["--account", "acc"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    assert!(text.contains("written NOTE/Note/a.pdf\n"), "{text}");
    assert!(
        text.contains("acc: written: 1, moved: 0, deleted: 0, failed: 0"),
        "{text}"
    );
    assert!(render.path().join("acc/NOTE/Note/a.pdf").exists());

    let again = std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
        .args(["cloud", "render", "--all", "--data"])
        .arg(data.path())
        .output()
        .unwrap();
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    assert!(stdout(&again).contains("written: 1"), "{}", stdout(&again));
}

#[test]
fn cloud_render_with_an_empty_render_dir_fails() {
    let data = render_fixture();
    std::fs::write(data.path().join("server.toml"), "render_dir = \"\"\n").unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
        .args(["cloud", "render", "--data"])
        .arg(data.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("rendering is off"), "{err}");
    assert!(!data.path().join("share").exists());
}

#[test]
fn cloud_render_without_render_dir_writes_into_share() {
    let data = render_fixture();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
        .args(["cloud", "render", "--data"])
        .arg(data.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(data.path().join("share/acc/NOTE/Note/a.pdf").is_file());
    assert!(!data.path().join("server.toml").exists());
}

#[test]
fn cloud_diagnose_prints_the_report_and_writes_the_zip() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let out = cloud(
        &data,
        &[
            "add",
            "jordi",
            "--email",
            "j@example.org",
            "--password-stdin",
        ],
        Some("pw\n"),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let zip = tmp.path().join("b.zip");
    let run = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
            .args(["cloud", "diagnose", "--data"])
            .arg(&data)
            .arg("--out")
            .arg(&zip)
            .output()
            .unwrap()
    };
    let out = run();
    let text = stdout(&out);
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.starts_with("notedown "), "{text}");
    assert!(text.contains("safe bundle"), "{text}");
    assert!(!text.contains("jordi"), "{text}");
    let last = text.lines().last().unwrap();
    assert!(
        last.starts_with(&format!("Bundle written: {}", zip.display())),
        "{text}"
    );
    assert!(zip.is_file());
    let again = run();
    assert_eq!(again.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&again.stderr).contains("already exists"));
    let bad = std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
        .args(["cloud", "diagnose", "--since", "soon", "--data"])
        .arg(&data)
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("--since"));
}

// ---- notedown pipeline and notedown mcp ----
#[cfg(feature = "ocr")]
mod pipeline_and_mcp {
    use super::*;

    const NOTE_A: &str = "F20240424175151947577oMie9HNQYnTa";
    const NOTE_B: &str = "F20240424175151947577oMie9HNQYnTb";

    fn text_block(id: &str, text: &str) -> notedown_pipeline::model::Block {
        notedown_pipeline::model::Block {
            id: id.into(),
            label: "text".into(),
            bbox: [0, 0, 10, 10],
            text: text.into(),
            replaces: Vec::new(),
        }
    }

    fn page_line(
        note: &str,
        path: &str,
        change: notedown_pipeline::model::LineChange,
        blocks: Vec<notedown_pipeline::model::Block>,
    ) -> notedown_pipeline::model::LogLine {
        let mut line = notedown_pipeline::model::LogLine::new(note, change);
        line.page_id = Some("P1".into());
        line.position = Some(0);
        line.path = Some(path.into());
        line.blocks = blocks;
        line
    }

    /// A data folder with the accounts `slugs`; the first one gets a text store seeded through
    /// the library:
    /// - seq 1: Planview new
    /// - seq 2: Garden new
    /// - seq 3: Planview edited
    fn pipeline_fixture(slugs: &[&str]) -> tempfile::TempDir {
        use notedown_pipeline::model::LineChange;
        use notedown_pipeline::store::{TextStore, WriterLock};
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        for (i, slug) in slugs.iter().enumerate() {
            notedown_cloud::auth::Accounts::new(data)
                .create(slug, &format!("{slug}{i}@example.org"), "secret")
                .unwrap();
        }
        let w = WriterLock::take(data).unwrap();
        let store = TextStore::open_writer(data, slugs[0], &w).unwrap();
        let mut lines = vec![
            page_line(
                NOTE_A,
                "NOTE/Note/Planview.note",
                LineChange::New,
                vec![text_block("b1", "Meeting with Anna on Friday")],
            ),
            page_line(
                NOTE_B,
                "NOTE/Note/Garden.note",
                LineChange::New,
                vec![text_block("b1", "Plant tomatoes in May")],
            ),
            page_line(
                NOTE_A,
                "NOTE/Note/Planview.note",
                LineChange::Edited,
                vec![text_block("b1", "Meeting with Anne on Friday")],
            ),
        ];
        store.append(&w, &mut lines).unwrap();
        store.repair(&w).unwrap();
        dir
    }

    fn pipeline_cmd(data: &std::path::Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
            .arg("pipeline")
            .args(args)
            .arg("--data")
            .arg(data)
            .env("NOTEDOWN_MODEL_DIR", data.join("no-model"))
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    #[test]
    fn pipeline_once_on_a_fresh_data_folder_says_so_and_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let out = pipeline_cmd(dir.path(), &["--once"]);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{err}");
        assert!(err.contains("no accounts in"), "{err}");
        assert!(err.contains("notedown cloud account add"), "{err}");
        assert!(!dir.path().join("pipeline").exists());
    }

    fn json_out(out: &std::process::Output) -> serde_json::Value {
        assert!(
            out.status.success(),
            "{}{}",
            stdout(out),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", stdout(out)))
    }

    /// `2026-10-08 23:14 +02:00`: a time at this computer's offset, as the MCP server shows it.
    fn assert_local_time(v: &serde_json::Value) {
        let t = v.as_str().unwrap_or_else(|| panic!("no time string: {v}"));
        let b = t.as_bytes();
        let digits = |r: std::ops::Range<usize>| r.into_iter().all(|i| b[i].is_ascii_digit());
        assert!(
            b.len() == 23
                && digits(0..4)
                && b[4] == b'-'
                && b[10] == b' '
                && b[13] == b':'
                && b[16] == b' '
                && (b[17] == b'+' || b[17] == b'-')
                && b[20] == b':',
            "{t}"
        );
    }

    #[test]
    fn pipeline_changes_and_ack() {
        let dir = pipeline_fixture(&["jordi"]);
        let d = dir.path();
        let first = json_out(&pipeline_cmd(
            d,
            &["changes", "--account", "jordi", "--consumer", "agent"],
        ));
        assert_eq!(first["from"], 0);
        assert_eq!(first["upTo"], 3);
        assert_eq!(first["more"], false);
        let notes = first["notes"].as_array().unwrap();
        assert_eq!(notes.len(), 2, "{first}");
        let text = first.to_string();
        assert!(text.contains("Meeting with Anne on Friday"), "{text}");
        assert!(text.contains("Plant tomatoes in May"), "{text}");

        // The account may be left out when there is only one; the manifest holds no text.
        let summary = json_out(&pipeline_cmd(
            d,
            &["changes", "--consumer", "agent", "--summary"],
        ));
        assert_eq!(summary["upTo"], 3);
        assert_eq!(summary["notes"].as_array().unwrap().len(), 2);
        assert!(!summary.to_string().contains("Meeting"), "{summary}");

        // A fixed range of one note.
        let range = json_out(&pipeline_cmd(
            d,
            &["changes", "--from", "0", "--up-to", "3", "--notes", NOTE_B],
        ));
        let notes = range["notes"].as_array().unwrap();
        assert_eq!(notes.len(), 1, "{range}");
        assert_eq!(notes[0]["key"], NOTE_B);

        // "since" a seq needs no bookmark.
        let since = json_out(&pipeline_cmd(d, &["changes", "--since", "2"]));
        assert_eq!(since["from"], 2);
        assert_eq!(since["notes"].as_array().unwrap().len(), 1, "{since}");

        let ack = json_out(&pipeline_cmd(
            d,
            &[
                "ack",
                "--account",
                "jordi",
                "--consumer",
                "agent",
                "--up-to",
                "3",
            ],
        ));
        assert_eq!(ack["seq"], 3);
        assert_local_time(&ack["updatedAt"]);
        assert!(ack.get("clamped").is_none(), "{ack}");
        // An upTo past the log stops at its end and says so.
        let past = json_out(&pipeline_cmd(
            d,
            &["ack", "--consumer", "agent", "--up-to", "99"],
        ));
        assert_eq!(past["seq"], 3, "{past}");
        assert_eq!(past["clamped"], true, "{past}");
        assert_eq!(past["logEnd"], 3, "{past}");

        let after = json_out(&pipeline_cmd(d, &["changes", "--consumer", "agent"]));
        assert_eq!(after["from"], 3);
        assert_eq!(after["notes"].as_array().unwrap().len(), 0, "{after}");

        // One start only, and notes need a fixed range.
        let both = pipeline_cmd(d, &["changes", "--consumer", "agent", "--since", "1"]);
        assert_eq!(both.status.code(), Some(2));
        let no_range = pipeline_cmd(d, &["changes", "--consumer", "agent", "--notes", NOTE_A]);
        assert_eq!(no_range.status.code(), Some(2));
        let unknown = pipeline_cmd(d, &["changes", "--account", "nobody", "--consumer", "x"]);
        assert_eq!(unknown.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&unknown.stderr).contains("no account nobody"));
        assert!(!d.join("pipeline/nobody").exists());
    }

    #[test]
    fn pipeline_refuses_a_second_writer() {
        let dir = pipeline_fixture(&["jordi"]);
        let d = dir.path();
        let lock = notedown_pipeline::store::WriterLock::take(d).unwrap();
        for args in [&["--once"][..], &["--reocr"][..], &[][..]] {
            let out = pipeline_cmd(d, args);
            assert_eq!(out.status.code(), Some(1), "{args:?}");
            assert!(
                String::from_utf8_lossy(&out.stderr)
                    .contains("another notedown pipeline runs for this data folder"),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            // A refused writer says only that.
            assert_eq!(
                String::from_utf8_lossy(&out.stderr).lines().count(),
                1,
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        drop(lock);
        // No page is due: the run needs no model and ends at once.
        let out = pipeline_cmd(d, &["--once"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(stdout(&out), "0 pages read, 0 versions written, 0 failed\n");
    }

    #[test]
    fn mcp_refuses_without_account_when_two_exist() {
        let dir = pipeline_fixture(&["jordi", "anna"]);
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
            .args(["mcp", "--data"])
            .arg(dir.path())
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(stdout(&out), "", "stdout belongs to the MCP protocol");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("this data folder has several accounts: anna, jordi"),
            "{err}"
        );
        assert!(err.contains("--account"), "{err}");
    }

    #[test]
    fn consumers_lists_and_removes() {
        let dir = pipeline_fixture(&["jordi", "anna"]);
        let d = dir.path();
        let ack = pipeline_cmd(
            d,
            &[
                "ack",
                "--account",
                "jordi",
                "--consumer",
                "agent",
                "--up-to",
                "2",
            ],
        );
        json_out(&ack);
        let list = json_out(&pipeline_cmd(d, &["consumers"]));
        assert_eq!(list["jordi"][0]["consumer"], "agent", "{list}");
        assert_eq!(list["jordi"][0]["seq"], 2, "{list}");
        assert_local_time(&list["jordi"][0]["updatedAt"]);
        assert_eq!(list["anna"], serde_json::json!([]), "{list}");
        let one = json_out(&pipeline_cmd(d, &["consumers", "--account", "anna"]));
        assert_eq!(one, serde_json::json!({"anna": []}));

        // Two accounts: removing needs the account.
        let no_account = pipeline_cmd(d, &["consumers", "--remove", "agent"]);
        assert_eq!(no_account.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&no_account.stderr).contains("several accounts"));

        let removed = pipeline_cmd(d, &["consumers", "--account", "jordi", "--remove", "agent"]);
        assert!(removed.status.success());
        assert_eq!(
            stdout(&removed),
            "removed the bookmark of agent in account jordi\n"
        );
        let list = json_out(&pipeline_cmd(d, &["consumers"]));
        assert_eq!(list["jordi"], serde_json::json!([]), "{list}");
        let again = pipeline_cmd(d, &["consumers", "--account", "jordi", "--remove", "agent"]);
        assert_eq!(again.status.code(), Some(1));
        assert!(
            String::from_utf8_lossy(&again.stderr).contains("no bookmark agent in account jordi"),
            "{}",
            String::from_utf8_lossy(&again.stderr)
        );
    }

    #[test]
    fn diagnose_reports_the_pipeline_line_and_leaves_its_texts_out() {
        use notedown_pipeline::model::PageOcrState;
        use notedown_pipeline::store::{TextStore, WriterLock};
        let dir = pipeline_fixture(&["jordi"]);
        let d = dir.path();
        {
            let w = WriterLock::take(d).unwrap();
            let store = TextStore::open_writer(d, "jordi", &w).unwrap();
            let mut a = store.note(NOTE_A).unwrap().unwrap();
            a.pages[0].state = PageOcrState::Failed;
            store.save_note(&w, &a).unwrap();
            let mut b = store.note(NOTE_B).unwrap().unwrap();
            let mut second = b.pages[0].clone();
            second.page_id = "P2".into();
            second.position = 1;
            second.state = PageOcrState::Pending;
            b.pages.push(second.clone());
            second.page_id = "P3".into();
            second.position = 2;
            b.pages.push(second);
            store.save_note(&w, &b).unwrap();
        }
        // A note state that cannot be read counts nothing and stops nothing.
        std::fs::write(d.join("pipeline/jordi/notes/broken.json"), "{").unwrap();
        notedown_pipeline::cursor::write(d, "jordi", 7).unwrap();
        let zip_path = dir.path().join("b.zip");
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
            .args(["cloud", "diagnose", "--data"])
            .arg(d)
            .arg("--out")
            .arg(&zip_path)
            .output()
            .unwrap();
        let text = stdout(&out);
        assert!(
            out.status.success(),
            "{text}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            text.contains("\nPipeline  cursor acct-1 7, 2 pages pending, 1 failed\n"),
            "{text}"
        );
        let mut zip = zip::ZipArchive::new(std::fs::File::open(&zip_path).unwrap()).unwrap();
        for i in 0..zip.len() {
            let mut f = zip.by_index(i).unwrap();
            let name = f.name().unwrap().into_owned();
            assert!(!name.starts_with("pipeline/"), "{name}");
            let mut body = Vec::new();
            std::io::Read::read_to_end(&mut f, &mut body).unwrap();
            let body = String::from_utf8_lossy(&body);
            for secret in ["Meeting with An", "Plant tomatoes", "Planview", "Garden"] {
                assert!(!body.contains(secret), "{secret} in {name}");
            }
        }

        // Without a pipeline the line says so in numbers too.
        let empty = tempfile::tempdir().unwrap();
        notedown_cloud::auth::Accounts::new(empty.path())
            .create("solo", "solo@example.org", "pw")
            .unwrap();
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
            .args(["cloud", "diagnose", "--data"])
            .arg(empty.path())
            .arg("--out")
            .arg(empty.path().join("b.zip"))
            .output()
            .unwrap();
        assert!(
            stdout(&out).contains("\nPipeline  not on this machine\n"),
            "{}",
            stdout(&out)
        );
    }

    #[test]
    fn serve_pipeline_starts_the_pipeline_and_stops_it_with_the_server() {
        use std::io::BufRead;
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        std::fs::write(data.join("server.toml"), "listen = [\"127.0.0.1:0\"]\n").unwrap();
        let mut serve = std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
            .args(["serve", "--pipeline", "--data"])
            .arg(data)
            .env("NOTEDOWN_MODEL_DIR", data.join("no-model"))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut reader = std::io::BufReader::new(serve.stdout.take().unwrap());
        let mut pid = None;
        loop {
            let mut line = String::new();
            assert!(
                reader.read_line(&mut line).unwrap() > 0,
                "serve ended early"
            );
            if let Some(rest) = line.strip_prefix("pipeline: started notedown pipeline (pid ") {
                pid = Some(rest.trim().trim_end_matches(')').to_string());
            }
            if line.starts_with("press Ctrl-C") {
                break;
            }
        }
        let pid = pid.expect("the start line names the pid");
        let alive = |pid: &str| {
            std::process::Command::new("kill")
                .args(["-0", pid])
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        // Give the child time to start; it waits for work and keeps running.
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(alive(&pid));
        let st = std::process::Command::new("kill")
            .args(["-TERM", &serve.id().to_string()])
            .status()
            .unwrap();
        assert!(st.success());
        let status = serve.wait().unwrap();
        assert!(status.success(), "{status:?}");
        assert!(!alive(&pid), "the pipeline child still runs");
        notedown_pipeline::store::WriterLock::take(data).unwrap();
    }

    #[test]
    fn pipeline_once_without_the_model_says_so_once() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        notedown_cloud::auth::Accounts::new(data)
            .create("jordi", "jordi@example.org", "secret")
            .unwrap();
        let journal = notedown_cloud::journal::Journal::open(data).unwrap();
        notedown_cloud::put::put(
            data,
            "jordi",
            &journal,
            std::path::Path::new(NOTE),
            "NOTE/Note",
            None,
        )
        .unwrap();
        let out = pipeline_cmd(data, &["--once"]);
        assert_eq!(out.status.code(), Some(1));
        let err = String::from_utf8_lossy(&out.stderr);
        let message = "the OCR model is missing, run `notedown model pull`";
        assert_eq!(err.matches(message).count(), 1, "{err}");
        assert_eq!(stdout(&out), "");
    }
}

#[cfg(not(feature = "ocr"))]
#[test]
fn the_pi_build_has_no_pipeline() {
    let help = stdout(&notedown(&["--help"]));
    let commands: Vec<&str> = help
        .lines()
        .filter_map(|l| {
            l.strip_prefix("  ")
                .and_then(|l| l.split_whitespace().next())
        })
        .collect();
    assert!(commands.contains(&"serve"));
    for gone in ["pipeline", "mcp", "convert", "model"] {
        assert!(!commands.contains(&gone), "{gone} is in the Pi build");
    }
}

#[test]
fn serve_prints_the_rest_api_line() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("server.toml"),
        "listen = [\"127.0.0.1:0\"]\n",
    )
    .unwrap();
    let out = serve_startup(dir.path(), &[]);
    let rest = out
        .lines()
        .find(|l| l.starts_with("REST API: "))
        .unwrap_or_else(|| panic!("no REST line: {out}"));
    let base = rest
        .strip_prefix("REST API: ")
        .and_then(|r| r.split("/rest/v1").next())
        .unwrap();
    assert!(base.starts_with("http://127.0.0.1:"), "{rest}");
    assert_eq!(
        rest,
        format!("REST API: {base}/rest/v1 (connect a pipeline with notedown connect {base})")
    );
    let at = |prefix: &str| out.lines().position(|l| l.starts_with(prefix));
    assert!(at("REST API: ") > at("listening on: "), "{out}");
}

/// Issues a REST token the way a login does: a code, then the hash of the password with it.
fn issue_token(
    data: &std::path::Path,
    email: &str,
    password: &str,
    name: &str,
    scope: notedown_cloud::auth::Scope,
) -> String {
    use notedown_cloud::auth::{Auth, expected_login_hash, md5_hex};
    let auth = Auth::open(data).unwrap();
    let (code, _) = auth.challenge(email, 1_000).unwrap();
    let hash = expected_login_hash(&md5_hex(password), &code);
    let (_, info, _) = auth.rest_login(email, &hash, name, scope, 2_000).unwrap();
    info.id
}

fn account_with_tokens() -> (tempfile::TempDir, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let out = cloud(
        d,
        &[
            "add",
            "jordi",
            "--email",
            "j@example.org",
            "--password-stdin",
        ],
        Some("secret\n"),
    );
    assert!(out.status.success());
    let first = issue_token(d, "j@example.org", "secret", "laptop", Scope::Read);
    let second = issue_token(d, "j@example.org", "secret", "studio", Scope::Read);
    (dir, first, second)
}

#[test]
fn tokens_list_and_revoke() {
    let (dir, first, second) = account_with_tokens();
    let d = dir.path();
    let out = cloud(d, &["tokens", "list", "jordi"], None);
    assert!(out.status.success());
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    // Both tokens have the same creation time (the test clock), so the list is in id order.
    let mut want = [(&first, "laptop"), (&second, "studio")];
    want.sort();
    for (line, (id, name)) in lines.iter().zip(want) {
        let rest = line
            .strip_prefix(&format!("{id}  read  {name}  created "))
            .unwrap();
        assert!(rest.starts_with("1970-01-01T00:00:02"), "{line}");
    }
    let out = cloud(d, &["tokens", "revoke", "jordi", &first], None);
    assert_eq!(stdout(&out), format!("revoked {first}\n"));
    let out = cloud(d, &["tokens", "revoke", "jordi", &first], None);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(&format!("no token {first} for jordi")),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let issued = issue_token(d, "j@example.org", "secret", "third", Scope::Read);
    assert_ne!(issued, second);
    let out = cloud(d, &["tokens", "revoke", "jordi", "--all"], None);
    assert_eq!(stdout(&out), "revoked 2 tokens\n");
    let out = cloud(d, &["tokens", "list", "jordi"], None);
    assert_eq!(stdout(&out), "no tokens\n");
}

#[test]
fn tokens_list_shows_the_scope() {
    let (dir, read, _) = account_with_tokens();
    let d = dir.path();
    let write = issue_token(d, "j@example.org", "secret", "mac app", Scope::Write);
    let out = cloud(d, &["tokens", "list", "jordi"], None);
    assert!(out.status.success());
    let text = stdout(&out);
    let line = |id: &str| {
        text.lines()
            .find(|l| l.starts_with(id))
            .unwrap()
            .to_string()
    };
    assert!(
        line(&read).starts_with(&format!("{read}  read  laptop  created ")),
        "{text}"
    );
    assert!(
        line(&write).starts_with(&format!("{write}  write  mac app  created ")),
        "{text}"
    );
}

#[test]
fn passwd_revokes_the_tokens() {
    let (dir, _, _) = account_with_tokens();
    let d = dir.path();
    let out = cloud(
        d,
        &["passwd", "jordi", "--password-stdin"],
        Some("other1\n"),
    );
    assert!(out.status.success());
    assert_eq!(
        stdout(&out),
        "password of jordi changed\nrevoked 2 REST tokens\n"
    );
    assert_eq!(
        stdout(&cloud(d, &["tokens", "list", "jordi"], None)),
        "no tokens\n"
    );
    let out = cloud(
        d,
        &["passwd", "jordi", "--password-stdin"],
        Some("other2\n"),
    );
    assert_eq!(stdout(&out), "password of jordi changed\n");
}

#[test]
fn revoke_refuses_a_bad_id() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let out = cloud(d, &["tokens", "revoke", "jordi", "../account"], None);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("token id"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (dir, _, _) = account_with_tokens();
    let out = cloud(
        dir.path(),
        &["tokens", "revoke", "jordi", "../account"],
        None,
    );
    assert!(!out.status.success());
    assert_eq!(
        stdout(&cloud(dir.path(), &["tokens", "list", "jordi"], None))
            .lines()
            .count(),
        2
    );
}

/// A running `notedown serve` on a random loopback port; it is killed on drop.
#[cfg(feature = "ocr")]
struct Server {
    child: std::process::Child,
    url: String,
}

#[cfg(feature = "ocr")]
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(feature = "ocr")]
fn running_server(data: &std::path::Path) -> Server {
    use std::io::BufRead;
    std::fs::write(data.join("server.toml"), "listen = [\"127.0.0.1:0\"]\n").unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
        .args(["serve", "--data"])
        .arg(data)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut url = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        if let Some(r) = line.strip_prefix("REST API: ") {
            url = r.split("/rest/v1").next().map(str::to_string);
        }
        if line.starts_with("press Ctrl-C") {
            break;
        }
    }
    // Keep reading so the server never blocks on a full pipe.
    std::thread::spawn(move || std::io::copy(&mut reader, &mut std::io::sink()).map(|_| ()));
    Server {
        child,
        url: url.expect("no REST line"),
    }
}

#[cfg(feature = "ocr")]
fn connect_cmd(url: &str, local: &std::path::Path, password: &str) -> std::process::Output {
    use std::io::Write;
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
        .args(["connect", url, "--data"])
        .arg(local)
        .args(["--email", "j@example.org", "--password-stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut pipe = child.stdin.take().unwrap();
    pipe.write_all(format!("{password}\n").as_bytes()).unwrap();
    drop(pipe);
    child.wait_with_output().unwrap()
}

#[cfg(feature = "ocr")]
fn server_with_account() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let out = cloud(
        dir.path(),
        &[
            "add",
            "jordi",
            "--email",
            "j@example.org",
            "--password-stdin",
        ],
        Some("secret\n"),
    );
    assert!(out.status.success());
    let server = running_server(dir.path());
    (dir, server)
}

#[cfg(feature = "ocr")]
#[test]
fn connect_logs_in_and_writes_pipeline_toml() {
    let (_data, server) = server_with_account();
    let local = tempfile::tempdir().unwrap();
    let out = connect_cmd(&server.url, local.path(), "secret");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    let file = local.path().join("tokens/jordi.token");
    let first = text.lines().next().unwrap();
    assert!(
        first.starts_with(&format!("connected to {} as jordi (user id ", server.url))
            && first.contains("), token ")
            && first.ends_with(&format!(" stored in {}", file.display())),
        "{text}"
    );
    assert_eq!(text.lines().count(), 1, "{text}");
    assert_eq!(
        std::fs::read_to_string(local.path().join("pipeline.toml")).unwrap(),
        format!("[server]\nurl = \"{}\"\n", server.url)
    );
    let token = std::fs::read_to_string(&file).unwrap();
    assert!(token.trim().starts_with("ndt1."));
    // The same URL with a trailing slash reconnects and names the older token.
    let out = connect_cmd(&format!("{}/", server.url), local.path(), "secret");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    assert!(
        text.contains("an older token ") && text.contains(" still works"),
        "{text}"
    );
    assert_ne!(std::fs::read_to_string(&file).unwrap(), token);
}

#[cfg(unix)]
#[cfg(feature = "ocr")]
#[test]
fn connect_stores_the_token_privately() {
    use std::os::unix::fs::PermissionsExt;
    let (_data, server) = server_with_account();
    let local = tempfile::tempdir().unwrap();
    let out = connect_cmd(&server.url, local.path(), "secret");
    assert!(out.status.success());
    let file = local.path().join("tokens/jordi.token");
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&file), 0o600);
    assert_eq!(mode(&local.path().join("tokens")), 0o700);
    let token = std::fs::read_to_string(&file).unwrap();
    let token = token.trim();
    assert!(!token.is_empty());
    for text in [
        stdout(&out),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ] {
        assert!(!text.contains(token), "the token is printed: {text}");
        assert!(!text.contains("secret"), "{text}");
    }
}

#[cfg(feature = "ocr")]
#[test]
fn connect_with_a_wrong_password_says_so() {
    let (_data, server) = server_with_account();
    let local = tempfile::tempdir().unwrap();
    let out = connect_cmd(&server.url, local.path(), "wrong");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.lines()
            .any(|l| l == "error: the server refused the email or the password"),
        "{err}"
    );
    assert!(!local.path().join("tokens").exists());
    assert!(!local.path().join("pipeline.toml").exists());
}

#[cfg(feature = "ocr")]
#[test]
fn connect_checks_the_folder_before_the_password() {
    let (data, server) = server_with_account();
    // An empty password is refused, but only after the folder is checked.
    let out = connect_cmd(&server.url, data.path(), "");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("holds a server's data"), "{err}");

    let local = tempfile::tempdir().unwrap();
    std::fs::write(
        local.path().join("pipeline.toml"),
        "[server]\nfolder = \"/srv/notedown\"\n",
    )
    .unwrap();
    let out = connect_cmd(&server.url, local.path(), "");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.lines().any(|l| l
            == "error: pipeline.toml points at the folder /srv/notedown; change it or use \
                another --data folder"),
        "{err}"
    );
    assert!(!local.path().join("tokens").exists());
}

#[cfg(feature = "ocr")]
#[test]
fn connect_refuses_a_url_with_a_user_name_or_a_password() {
    let (_data, server) = server_with_account();
    let local = tempfile::tempdir().unwrap();
    let url = server.url.replace("http://", "http://jordi:hidden@");
    let out = connect_cmd(&url, local.path(), "secret");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.lines().any(|l| l
            == "error: the server url must not hold a user name or a password; connect asks \
                for the password"),
        "{err}"
    );
    assert!(!err.contains("hidden"), "{err}");
    assert!(!local.path().join("tokens").exists());
    assert!(!local.path().join("pipeline.toml").exists());
}

#[cfg(feature = "ocr")]
#[test]
fn connect_refuses_another_url_in_pipeline_toml() {
    let (_data, server) = server_with_account();
    let local = tempfile::tempdir().unwrap();
    std::fs::write(
        local.path().join("pipeline.toml"),
        "[server]\nurl = \"http://other.example:1\"\n",
    )
    .unwrap();
    let out = connect_cmd(&server.url, local.path(), "secret");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(
            "pipeline.toml points at http://other.example:1; change it or use another --data folder"
        ),
        "{err}"
    );
    assert!(!local.path().join("tokens").exists());
}

#[cfg(feature = "ocr")]
#[test]
fn connect_refuses_a_server_data_folder() {
    let (data, server) = server_with_account();
    let out = connect_cmd(&server.url, data.path(), "secret");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("holds a server's data"), "{err}");
    assert!(!data.path().join("pipeline.toml").exists());
    assert!(!data.path().join("tokens").exists());
}
