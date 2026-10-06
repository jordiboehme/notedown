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
