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
