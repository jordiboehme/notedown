use crate::error::OcrError;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy)]
pub struct ModelFile {
    pub file_name: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct ModelSpec {
    pub label: &'static str,
    pub repo: &'static str,
    pub model: ModelFile,
    pub mmproj: ModelFile,
}

pub const DEFAULT_MODEL: ModelSpec = ModelSpec {
    label: "chandra-ocr-2 Q4_K_M",
    repo: "prithivMLmods/chandra-ocr-2-GGUF",
    model: ModelFile {
        file_name: "chandra-ocr-2.Q4_K_M.gguf",
        size: 3066384992,
        sha256: "bde0505fda04eb1a89e122b2112e60b10c8c9697db5ca90a2e03635984f3a07d",
    },
    mmproj: ModelFile {
        file_name: "chandra-ocr-2.mmproj-q8_0.gguf",
        size: 366894368,
        sha256: "709be8aeda77c5732e58e957367439eae14ebb73b4f4a78b558013d000948bc3",
    },
};

#[derive(Debug, Clone)]
pub struct ModelPaths {
    pub model: PathBuf,
    pub mmproj: PathBuf,
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

pub fn resolve(
    spec: &ModelSpec,
    model_override: Option<&Path>,
    mmproj_override: Option<&Path>,
) -> Result<ModelPaths, OcrError> {
    resolve_in(&cache_dir()?, spec, model_override, mmproj_override)
}

fn resolve_in(
    cache: &Path,
    spec: &ModelSpec,
    model_override: Option<&Path>,
    mmproj_override: Option<&Path>,
) -> Result<ModelPaths, OcrError> {
    let model = model_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cache.join(spec.model.file_name));
    let mmproj = mmproj_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cache.join(spec.mmproj.file_name));
    for path in [&model, &mmproj] {
        if !path.is_file() {
            return Err(OcrError::ModelMissing { path: path.clone() });
        }
    }
    let label = match model_override {
        Some(p) => p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| spec.label.to_string()),
        None => spec.label.to_string(),
    };
    Ok(ModelPaths {
        model,
        mmproj,
        label,
    })
}

/// Checks size first, then streams the file through SHA-256.
pub fn verify_file(path: &Path, expected: &ModelFile) -> Result<(), OcrError> {
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

/// Downloads every file that is missing or fails verification.
/// `progress` receives (file name, bytes so far, total bytes).
pub fn pull(
    spec: &ModelSpec,
    progress: &mut dyn FnMut(&str, u64, u64),
) -> Result<ModelPaths, OcrError> {
    let dir = cache_dir()?;
    std::fs::create_dir_all(&dir)?;
    for file in [&spec.model, &spec.mmproj] {
        let dest = dir.join(file.file_name);
        if verify_file(&dest, file).is_ok() {
            continue;
        }
        download(spec.repo, file, &dest, progress)?;
    }
    resolve_in(&dir, spec, None, None)
}

fn download(
    repo: &str,
    file: &ModelFile,
    dest: &Path,
    progress: &mut dyn FnMut(&str, u64, u64),
) -> Result<(), OcrError> {
    let url = format!(
        "https://huggingface.co/{repo}/resolve/main/{}",
        file.file_name
    );
    download_from(&url, file, dest, progress)
}

/// Downloads `url` into `dest` through `dest.part`. A part left by an earlier run is
/// continued with a `Range` request. The part stays on download and IO errors and is
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

    if let Err(e) = verify_file(&part, file) {
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
        verify_file(&p, &good).unwrap();
        let bad = ModelFile {
            sha256: "00",
            ..good
        };
        assert!(matches!(
            verify_file(&p, &bad),
            Err(OcrError::Checksum { .. })
        ));
        let wrong_size = ModelFile { size: 6, ..good };
        assert!(matches!(
            verify_file(&p, &wrong_size),
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
            verify_file(&dir.path().join("nope"), &f),
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
    fn resolve_reports_missing_with_pull_hint() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_in(dir.path(), &DEFAULT_MODEL, None, None).unwrap_err();
        assert!(matches!(err, OcrError::ModelMissing { .. }));
        assert!(err.to_string().contains("notedown model pull"), "{err}");
    }

    #[test]
    fn resolve_uses_cache_paths_and_default_label() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(DEFAULT_MODEL.model.file_name), b"x").unwrap();
        std::fs::write(dir.path().join(DEFAULT_MODEL.mmproj.file_name), b"y").unwrap();
        let r = resolve_in(dir.path(), &DEFAULT_MODEL, None, None).unwrap();
        assert_eq!(r.model, dir.path().join(DEFAULT_MODEL.model.file_name));
        assert_eq!(r.label, "chandra-ocr-2 Q4_K_M");
    }

    #[test]
    fn overrides_are_used_as_given() {
        let dir = tempfile::tempdir().unwrap();
        let m = dir.path().join("my-q8.gguf");
        let p = dir.path().join("proj.gguf");
        std::fs::write(&m, b"x").unwrap();
        std::fs::write(&p, b"y").unwrap();
        let empty = tempfile::tempdir().unwrap();
        let r = resolve_in(empty.path(), &DEFAULT_MODEL, Some(&m), Some(&p)).unwrap();
        assert_eq!(r.model, m);
        assert_eq!(r.mmproj, p);
        assert_eq!(r.label, "my-q8");
    }

    #[test]
    #[ignore]
    fn real_cache_files_verify() {
        let dir = cache_dir().unwrap();
        verify_file(
            &dir.join(DEFAULT_MODEL.model.file_name),
            &DEFAULT_MODEL.model,
        )
        .unwrap();
        verify_file(
            &dir.join(DEFAULT_MODEL.mmproj.file_name),
            &DEFAULT_MODEL.mmproj,
        )
        .unwrap();
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
                    _ => format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    ),
                };
                let payload = match (mode, start) {
                    (Serve::Ranges, Some(s)) => &body[s..],
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
}
