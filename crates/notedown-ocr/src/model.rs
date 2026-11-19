// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::error::OcrError;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub use pagevision::model::{HubFile, ModelFiles, hub_url};

/// The four model files of pagevision, each pinned to a Hugging Face revision: the Qwen model,
/// its mmproj, the layout detector and the text line detector.
pub const FILES: [HubFile; 4] = pagevision::model::ALL_FILES;

/// The model name stored with every page and file this engine reads.
pub const ENGINE_LABEL: &str = "pagevision qwen3.5-4b Q4_K_M";

/// The size of all four files together (3,556,512,484 bytes).
pub const TOTAL_SIZE: &str = "about 3.6 GB";

/// The files of the Chandra model that notedown 0.1 downloaded. `clean_old` removes them and
/// their part files; nothing removes them on its own.
pub const OLD_FILES: [&str; 2] = [
    "chandra-ocr-2.Q4_K_M.gguf",
    "chandra-ocr-2.mmproj-q8_0.gguf",
];

/// One file to download, with its expected size and SHA-256.
#[derive(Debug, Clone, Copy)]
pub struct ModelFile {
    pub file_name: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

impl From<&HubFile> for ModelFile {
    fn from(f: &HubFile) -> Self {
        ModelFile {
            file_name: f.local,
            size: f.size,
            sha256: f.sha256,
        }
    }
}

/// The four model files to load, and the model name to store.
#[derive(Debug, Clone)]
pub struct ModelPaths {
    pub files: ModelFiles,
    pub label: String,
}

pub fn cache_dir() -> Result<PathBuf, OcrError> {
    cache_dir_from(std::env::var_os("NOTEDOWN_MODEL_DIR"))
}

fn cache_dir_from(env: Option<std::ffi::OsString>) -> Result<PathBuf, OcrError> {
    if let Some(v) = env.filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(v));
    }
    dirs::cache_dir()
        .map(|d| d.join("notedown").join("models"))
        .ok_or_else(|| OcrError::Engine("cannot determine the cache directory".into()))
}

/// The four files in the cache folder. `--model` and `--mmproj` replace only the Qwen files.
/// Every file must exist.
pub fn resolve(
    model_override: Option<&Path>,
    mmproj_override: Option<&Path>,
) -> Result<ModelPaths, OcrError> {
    resolve_in(&cache_dir()?, model_override, mmproj_override)
}

fn resolve_in(
    cache: &Path,
    model_override: Option<&Path>,
    mmproj_override: Option<&Path>,
) -> Result<ModelPaths, OcrError> {
    let mut files = ModelFiles::in_dir(cache);
    if let Some(p) = model_override {
        files.model = p.to_path_buf();
    }
    if let Some(p) = mmproj_override {
        files.mmproj = p.to_path_buf();
    }
    for path in [&files.model, &files.mmproj, &files.layout, &files.text_det] {
        if !path.is_file() {
            return Err(OcrError::ModelMissing { path: path.clone() });
        }
    }
    Ok(ModelPaths {
        files,
        label: override_label(model_override, ENGINE_LABEL),
    })
}

/// The file stem of a `--model` override, else `default`.
fn override_label(model_override: Option<&Path>, default: &str) -> String {
    model_override
        .and_then(Path::file_stem)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| default.to_string())
}

/// The licences of the four files, one line per licensor. Shown before a download starts.
pub fn licence_lines() -> Vec<String> {
    let [model, _, layout, _] = FILES;
    vec![
        format!("Qwen3.5-4B ({}): {}", model.licensor, model.licence),
        format!(
            "PP-DocLayoutV3 and PP-OCRv6 ({}): {}",
            layout.licensor, layout.licence
        ),
    ]
}

/// Removes the Chandra files of notedown 0.1 (`OLD_FILES`) and their part files from `dir`, and
/// nothing else. Returns the removed paths. A missing file or folder is no error.
pub fn clean_old(dir: &Path) -> Result<Vec<PathBuf>, OcrError> {
    let mut removed = Vec::new();
    for name in OLD_FILES {
        for path in [dir.join(name), dir.join(format!("{name}.part"))] {
            match std::fs::remove_file(&path) {
                Ok(()) => removed.push(path),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(removed)
}

/// Checks size first, then streams the file through SHA-256.
pub fn verify_file(path: &Path, expected: &HubFile) -> Result<(), OcrError> {
    verify_model_file(path, &expected.into())
}

fn verify_model_file(path: &Path, expected: &ModelFile) -> Result<(), OcrError> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(OcrError::ModelMissing {
                path: path.to_path_buf(),
            });
        }
        Err(e) => return Err(e.into()),
    };
    if meta.len() != expected.size {
        return Err(OcrError::Checksum {
            path: path.to_path_buf(),
            expected: format!("{} bytes", expected.size),
            actual: format!("{} bytes", meta.len()),
        });
    }
    let actual = hash_reader(std::fs::File::open(path)?)?;
    check_hash(path, expected, actual)
}

fn hash_reader(mut r: impl Read) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(const_hex::encode(hasher.finalize()))
}

fn check_hash(path: &Path, expected: &ModelFile, actual: String) -> Result<(), OcrError> {
    if actual.eq_ignore_ascii_case(expected.sha256) {
        Ok(())
    } else {
        Err(OcrError::Checksum {
            path: path.to_path_buf(),
            expected: expected.sha256.to_string(),
            actual,
        })
    }
}

/// Downloads every one of the four files that is missing or fails verification, each from its
/// pinned revision (`hub_url`) into `<cache>/<local name>`.
/// `progress` receives (file name, bytes so far, total bytes).
pub fn pull(progress: &mut dyn FnMut(&str, u64, u64)) -> Result<ModelPaths, OcrError> {
    pull_in(&cache_dir()?, &mut download_from, progress)
}

/// The download of one file: URL, expected file, destination and progress.
type Fetch<'a> =
    dyn FnMut(&str, &ModelFile, &Path, &mut dyn FnMut(&str, u64, u64)) -> Result<(), OcrError> + 'a;

/// [`pull`] into `dir`, with the download as a seam. A file that is there already reports
/// itself as complete.
fn pull_in(
    dir: &Path,
    fetch: &mut Fetch,
    progress: &mut dyn FnMut(&str, u64, u64),
) -> Result<ModelPaths, OcrError> {
    std::fs::create_dir_all(dir)?;
    for f in &FILES {
        let dest = dir.join(f.local);
        if verify_file(&dest, f).is_ok() {
            progress(f.local, f.size, f.size);
            continue;
        }
        fetch(&hub_url(f), &f.into(), &dest, progress)?;
    }
    resolve_in(dir, None, None)
}

/// Downloads `url` into `dest` through `dest.part`. A part left by an earlier run is
/// continued with a `Range` request; a partial answer from another offset is asked for again
/// without `Range`. The part stays on download and IO errors and is
/// removed only when the complete file fails the size or SHA-256 check.
/// `progress` reports the bytes on disk and the size of the whole file.
pub fn download_from(
    url: &str,
    file: &ModelFile,
    dest: &Path,
    progress: &mut dyn FnMut(&str, u64, u64),
) -> Result<(), OcrError> {
    let mut part_name = dest.as_os_str().to_owned();
    part_name.push(".part");
    let part = PathBuf::from(part_name);
    let total = file.size;
    let fail = |e: reqwest::Error| OcrError::Download(format!("{url}: {e}"));

    let mut have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if have > total {
        std::fs::remove_file(&part)?;
        have = 0;
    }

    if have < total {
        let client = reqwest::blocking::Client::builder()
            .timeout(None)
            .build()
            .map_err(fail)?;
        let mut request = client.get(url);
        if have > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
        }
        let mut resp = request
            .send()
            .and_then(|r| r.error_for_status())
            .map_err(fail)?;
        let resumes = have > 0
            && resp.status() == reqwest::StatusCode::PARTIAL_CONTENT
            && resp
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with(&format!("bytes {have}-")));
        if have > 0 && !resumes && resp.status() == reqwest::StatusCode::PARTIAL_CONTENT {
            // A part of the file from another offset: ask for the whole file instead of
            // writing those bytes at the start.
            drop(resp);
            resp = client
                .get(url)
                .send()
                .and_then(|r| r.error_for_status())
                .map_err(fail)?;
            if resp.status() == reqwest::StatusCode::PARTIAL_CONTENT {
                return Err(OcrError::Download(format!(
                    "{url}: the server answers with a part of the file to a request for all of it"
                )));
            }
        }
        let mut out = if resumes {
            std::fs::OpenOptions::new().append(true).open(&part)?
        } else {
            have = 0;
            std::fs::File::create(&part)?
        };
        let mut buf = vec![0u8; 1 << 20];
        let mut done = have;
        progress(file.file_name, done, total);
        loop {
            let n = resp
                .read(&mut buf)
                .map_err(|e| OcrError::Download(format!("{url}: {e}")))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            done += n as u64;
            progress(file.file_name, done, total);
        }
        out.flush()?;
        drop(out);
        if done < total {
            return Err(OcrError::Download(format!(
                "{url}: the connection ended after {done} of {total} bytes"
            )));
        }
    } else {
        progress(file.file_name, total, total);
    }

    if let Err(e) = verify_model_file(&part, file) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::rename(&part, dest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_detects_mismatch_and_accepts_match() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.bin");
        std::fs::write(&p, b"hello").unwrap();
        let good = ModelFile {
            file_name: "f.bin",
            size: 5,
            sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
        };
        verify_model_file(&p, &good).unwrap();
        let bad = ModelFile {
            sha256: "00",
            ..good
        };
        assert!(matches!(
            verify_model_file(&p, &bad),
            Err(OcrError::Checksum { .. })
        ));
        let wrong_size = ModelFile { size: 6, ..good };
        assert!(matches!(
            verify_model_file(&p, &wrong_size),
            Err(OcrError::Checksum { .. })
        ));
    }

    #[test]
    fn verify_missing_file_is_model_missing() {
        let dir = tempfile::tempdir().unwrap();
        let f = ModelFile {
            file_name: "x",
            size: 1,
            sha256: "00",
        };
        assert!(matches!(
            verify_model_file(&dir.path().join("nope"), &f),
            Err(OcrError::ModelMissing { .. })
        ));
    }

    #[test]
    fn cache_dir_env_wins_and_default_ends_with_notedown_models() {
        let p = cache_dir_from(Some("/tmp/custom".into())).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/custom"));
        let d = cache_dir_from(None).unwrap();
        assert!(d.ends_with("notedown/models"), "{d:?}");
        // empty value counts as unset
        assert!(
            cache_dir_from(Some("".into()))
                .unwrap()
                .ends_with("notedown/models")
        );
    }

    #[test]
    #[ignore = "needs the four model files in the cache (notedown model pull)"]
    fn real_cache_files_verify() {
        let dir = cache_dir().unwrap();
        for f in &FILES {
            verify_file(&dir.join(f.local), f).unwrap();
        }
    }

    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Copy)]
    enum Serve {
        /// Answers 206 for a Range request.
        Ranges,
        /// Answers 200 with the whole file whatever the request says.
        Whole,
        /// Sends only the first n bytes of the whole file, then closes.
        Cut(usize),
        /// Answers a Range request with 206 from byte n (not the asked start), and a request
        /// without Range with 200 and the whole file.
        WrongRange(usize),
    }

    /// Serves `body` to the next requests. Returns the URL and the Range headers seen.
    fn serve(body: Vec<u8>, mode: Serve) -> (String, Arc<Mutex<Vec<Option<String>>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/f.bin", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut range = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        range = Some(v.trim().to_string());
                    }
                }
                log.lock().unwrap().push(range.clone());
                let start = range
                    .as_ref()
                    .and_then(|r| r.trim_end_matches('-').parse::<usize>().ok());
                let head = match (mode, start) {
                    (Serve::Ranges, Some(s)) => format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {s}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len() - 1,
                        body.len(),
                        body.len() - s
                    ),
                    (Serve::WrongRange(n), Some(_)) => format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {n}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len() - 1,
                        body.len(),
                        body.len() - n
                    ),
                    _ => format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    ),
                };
                let payload = match (mode, start) {
                    (Serve::Ranges, Some(s)) => &body[s..],
                    (Serve::WrongRange(n), Some(_)) => &body[n..],
                    (Serve::Cut(n), _) => &body[..n],
                    _ => &body[..],
                };
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(payload);
            }
        });
        (url, seen)
    }

    fn model_file(body: &[u8]) -> ModelFile {
        let hash = const_hex::encode(Sha256::digest(body));
        ModelFile {
            file_name: "f.bin",
            size: body.len() as u64,
            sha256: Box::leak(hash.into_boxed_str()),
        }
    }

    fn body() -> Vec<u8> {
        (0..4000u32).map(|i| (i % 251) as u8).collect()
    }

    fn part_of(dest: &Path) -> PathBuf {
        PathBuf::from(format!("{}.part", dest.display()))
    }

    #[test]
    fn a_pull_resumes_from_the_part_file() {
        let body = body();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("f.bin");
        std::fs::write(part_of(&dest), &body[..1500]).unwrap();
        let (url, seen) = serve(body.clone(), Serve::Ranges);
        download_from(&url, &model_file(&body), &dest, &mut |_, _, _| {}).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert!(!part_of(&dest).exists());
        assert_eq!(seen.lock().unwrap().as_slice(), [Some("1500-".to_string())]);
    }

    #[test]
    fn a_pull_restarts_when_the_server_ignores_the_range() {
        let body = body();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("f.bin");
        std::fs::write(part_of(&dest), &body[..1500]).unwrap();
        let (url, _) = serve(body.clone(), Serve::Whole);
        download_from(&url, &model_file(&body), &dest, &mut |_, _, _| {}).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
    }

    #[test]
    fn a_complete_part_is_not_downloaded_again() {
        let body = body();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("f.bin");
        std::fs::write(part_of(&dest), &body).unwrap();
        let (url, seen) = serve(body.clone(), Serve::Ranges);
        download_from(&url, &model_file(&body), &dest, &mut |_, _, _| {}).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn a_part_longer_than_the_file_starts_again() {
        let body = body();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("f.bin");
        std::fs::write(part_of(&dest), vec![7u8; body.len() + 10]).unwrap();
        let (url, seen) = serve(body.clone(), Serve::Ranges);
        download_from(&url, &model_file(&body), &dest, &mut |_, _, _| {}).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert_eq!(seen.lock().unwrap().as_slice(), [None]);
    }

    #[test]
    fn a_cut_download_keeps_the_part() {
        let body = body();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("f.bin");
        let (url, _) = serve(body.clone(), Serve::Cut(1000));
        let err = download_from(&url, &model_file(&body), &dest, &mut |_, _, _| {});
        assert!(matches!(err, Err(OcrError::Download(_))), "{err:?}");
        assert!(!dest.exists());
        assert_eq!(std::fs::read(part_of(&dest)).unwrap(), &body[..1000]);
    }

    #[test]
    fn a_resume_reports_the_full_total() {
        let body = body();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("f.bin");
        std::fs::write(part_of(&dest), &body[..1500]).unwrap();
        let (url, _) = serve(body.clone(), Serve::Ranges);
        let mut seen = Vec::new();
        download_from(&url, &model_file(&body), &dest, &mut |f, d, t| {
            seen.push((f.to_string(), d, t))
        })
        .unwrap();
        assert_eq!(seen.first().unwrap(), &("f.bin".to_string(), 1500, 4000));
        assert_eq!(seen.last().unwrap(), &("f.bin".to_string(), 4000, 4000));
    }

    #[test]
    fn a_resumed_file_with_wrong_bytes_fails_and_removes_the_part() {
        let body = body();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("f.bin");
        std::fs::write(part_of(&dest), vec![9u8; 1500]).unwrap();
        let (url, _) = serve(body.clone(), Serve::Ranges);
        let err = download_from(&url, &model_file(&body), &dest, &mut |_, _, _| {});
        assert!(matches!(err, Err(OcrError::Checksum { .. })), "{err:?}");
        assert!(!part_of(&dest).exists());
        assert!(!dest.exists());
    }

    #[test]
    fn a_complete_part_with_wrong_bytes_fails_without_a_request() {
        let body = body();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("f.bin");
        std::fs::write(part_of(&dest), vec![9u8; body.len()]).unwrap();
        let (url, seen) = serve(body.clone(), Serve::Ranges);
        let err = download_from(&url, &model_file(&body), &dest, &mut |_, _, _| {});
        assert!(matches!(err, Err(OcrError::Checksum { .. })), "{err:?}");
        assert!(!part_of(&dest).exists());
        assert!(!dest.exists());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn a_partial_answer_from_another_offset_asks_again_without_a_range() {
        let body = body();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("f.bin");
        std::fs::write(part_of(&dest), &body[..1500]).unwrap();
        let (url, seen) = serve(body.clone(), Serve::WrongRange(1000));
        download_from(&url, &model_file(&body), &dest, &mut |_, _, _| {}).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [Some("1500-".to_string()), None]
        );
    }
}

#[cfg(test)]
mod four_file_tests {
    use super::*;

    #[test]
    fn pull_url_uses_the_revision() {
        let dir = tempfile::tempdir().unwrap();
        let mut urls = Vec::new();
        let mut fetch = |url: &str,
                         file: &ModelFile,
                         dest: &Path,
                         _: &mut dyn FnMut(&str, u64, u64)|
         -> Result<(), OcrError> {
            urls.push((url.to_string(), file.file_name.to_string()));
            std::fs::write(dest, b"x")?;
            Ok(())
        };
        let paths = pull_in(dir.path(), &mut fetch, &mut |_, _, _| {}).unwrap();
        let expected: Vec<(String, String)> = FILES
            .iter()
            .map(|f| (hub_url(f), f.local.to_string()))
            .collect();
        assert_eq!(urls, expected);
        for (url, _) in &urls {
            assert!(!url.contains("/resolve/main/"), "{url}");
        }
        assert_eq!(paths.files, ModelFiles::in_dir(dir.path()));
        assert_eq!(paths.label, ENGINE_LABEL);
    }

    #[test]
    fn resolve_needs_all_four_files() {
        let dir = tempfile::tempdir().unwrap();
        for (i, f) in FILES.iter().enumerate() {
            let err = resolve_in(dir.path(), None, None).unwrap_err();
            match err {
                OcrError::ModelMissing { path } => assert_eq!(path, dir.path().join(f.local)),
                e => panic!("{e}"),
            }
            std::fs::write(dir.path().join(f.local), b"x").unwrap();
            if i + 1 < FILES.len() {
                assert!(resolve_in(dir.path(), None, None).is_err());
            }
        }
        let r = resolve_in(dir.path(), None, None).unwrap();
        assert_eq!(r.files, ModelFiles::in_dir(dir.path()));
        assert_eq!(r.label, ENGINE_LABEL);
    }

    #[test]
    fn overrides_replace_only_the_qwen_files() {
        let dir = tempfile::tempdir().unwrap();
        for f in &FILES[2..] {
            std::fs::write(dir.path().join(f.local), b"x").unwrap();
        }
        let other = tempfile::tempdir().unwrap();
        let m = other.path().join("my-q8.gguf");
        let p = other.path().join("proj.gguf");
        std::fs::write(&m, b"x").unwrap();
        std::fs::write(&p, b"y").unwrap();
        let r = resolve_in(dir.path(), Some(&m), Some(&p)).unwrap();
        assert_eq!(r.files.model, m);
        assert_eq!(r.files.mmproj, p);
        assert_eq!(r.files.layout, dir.path().join(FILES[2].local));
        assert_eq!(r.files.text_det, dir.path().join(FILES[3].local));
        assert_eq!(r.label, "my-q8");
    }

    #[test]
    fn missing_file_names_the_pull_command() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_in(dir.path(), None, None).unwrap_err();
        assert!(err.to_string().contains("notedown model pull"), "{err}");
    }

    #[test]
    fn licences_name_the_licensors() {
        assert_eq!(
            licence_lines(),
            [
                "Qwen3.5-4B (Qwen team, Alibaba Cloud): Apache-2.0",
                "PP-DocLayoutV3 and PP-OCRv6 (PaddlePaddle Authors): Apache-2.0",
            ]
        );
        // Both detectors have the same licensor and licence, so one line covers them.
        assert_eq!(FILES[2].licensor, FILES[3].licensor);
        assert_eq!(FILES[2].licence, FILES[3].licence);
        assert_eq!(FILES[0].licensor, FILES[1].licensor);
    }

    #[test]
    fn total_size_text_matches_the_files() {
        let total: u64 = FILES.iter().map(|f| f.size).sum();
        assert_eq!(total, 3_556_512_484);
        assert_eq!(TOTAL_SIZE, "about 3.6 GB");
    }

    #[test]
    fn verify_names_each_file() {
        let dir = tempfile::tempdir().unwrap();
        for f in &FILES {
            let err = verify_file(&dir.path().join(f.local), f).unwrap_err();
            assert!(err.to_string().contains(f.local), "{err}");
        }
        let f = HubFile {
            size: 5,
            sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            ..FILES[3]
        };
        let p = dir.path().join(f.local);
        std::fs::write(&p, b"hello").unwrap();
        verify_file(&p, &f).unwrap();
    }

    #[test]
    fn clean_old_removes_only_chandra_files() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("chandra-ocr-2.Q4_K_M.gguf");
        let part = dir.path().join("chandra-ocr-2.mmproj-q8_0.gguf.part");
        let keep = dir.path().join("mmproj-F16.gguf");
        let other = dir.path().join("chandra-ocr-2.notes.txt");
        for p in [&old, &part, &keep, &other] {
            std::fs::write(p, b"x").unwrap();
        }
        let mut removed = clean_old(dir.path()).unwrap();
        removed.sort();
        assert_eq!(removed, [old.clone(), part.clone()]);
        assert!(!old.exists() && !part.exists());
        assert!(keep.exists() && other.exists());
        // Nothing left to remove is no error.
        assert!(clean_old(dir.path()).unwrap().is_empty());
        assert!(clean_old(&dir.path().join("missing")).unwrap().is_empty());
    }
}
