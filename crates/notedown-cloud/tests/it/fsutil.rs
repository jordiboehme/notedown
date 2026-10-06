//! Durable atomic writes.

use notedown_cloud::fsutil::{fsync_dir, rename_durable, write_atomic};

#[test]
fn concurrent_writers_to_one_path_never_tear_or_lose_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("target.json");
    let payloads: Vec<Vec<u8>> = (0..8u8).map(|i| vec![b'a' + i; 256 * 1024]).collect();
    std::thread::scope(|s| {
        for p in &payloads {
            let path = path.clone();
            s.spawn(move || {
                for _ in 0..25 {
                    write_atomic(&path, p, false).unwrap();
                }
            });
        }
        // a reader sees only whole payloads while the writers run
        let reader_path = path.clone();
        let payloads = &payloads;
        s.spawn(move || {
            for _ in 0..200 {
                if let Ok(bytes) = std::fs::read(&reader_path) {
                    assert!(
                        payloads.contains(&bytes),
                        "torn file of {} bytes",
                        bytes.len()
                    );
                }
            }
        });
    });
    let last = std::fs::read(&path).unwrap();
    assert!(payloads.contains(&last));
    // no temp file is left behind
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, vec!["target.json".to_string()]);
}

#[test]
fn temp_names_are_unique_and_a_failed_write_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    // the target is a non-empty folder: the rename fails and the temp file is removed
    let target = dir.path().join("t");
    std::fs::create_dir_all(target.join("x")).unwrap();
    assert!(write_atomic(&target, b"data", false).is_err());
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, vec!["t".to_string()]);
}

#[cfg(unix)]
#[test]
fn private_files_are_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("k");
    write_atomic(&path, b"x", true).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn rename_durable_moves_across_folders_and_fsync_dir_works() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("a")).unwrap();
    std::fs::create_dir(dir.path().join("b")).unwrap();
    let from = dir.path().join("a/f");
    let to = dir.path().join("b/g");
    std::fs::write(&from, b"1").unwrap();
    rename_durable(&from, &to).unwrap();
    assert!(!from.exists());
    assert_eq!(std::fs::read(&to).unwrap(), b"1");
    fsync_dir(dir.path()).unwrap();
    assert!(fsync_dir(&dir.path().join("missing")).is_err() || cfg!(not(unix)));
    assert!(rename_durable(&from, &to).is_err());
}
