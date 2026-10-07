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
    use std::io::BufRead;
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_notedown"))
        .args(["serve", "--data"])
        .arg(data)
        .args(extra)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
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
    lines
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
            "\npush mode: reconnect (the device's socket is closed, it syncs when it connects again)\n"
        ),
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

    let out = journal(d, &["snapshot", "--account", "jordi"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "journaled 3 events for account jordi (seq 2 to 4)\n"
    );
    let ev = lines(&journal(d, &["tail"]));
    assert_eq!(ev.len(), 4);
    let types: Vec<&str> = ev.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        [
            "folder.created",
            "folder.created",
            "folder.created",
            "file.created"
        ]
    );
    assert_eq!(ev[3]["path"], "INBOX/a.pdf");
    assert_eq!(ev[3]["kind"], "document");
    assert_eq!(ev[3]["origin"], "snapshot");
    let ev = lines(&journal(d, &["tail", "--from", "3"]));
    assert_eq!(
        ev.iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [3, 4]
    );
    let out = journal(d, &["snapshot", "--account", "ghost"]);
    assert!(!out.status.success());
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
