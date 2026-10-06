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
    let mut resp = reqwest::blocking::Client::builder()
        .timeout(None)
        .build()
        .and_then(|c| c.get(&url).send())
        .and_then(|r| r.error_for_status())
        .map_err(|e| OcrError::Download(format!("{url}: {e}")))?;
    let total = resp.content_length().unwrap_or(file.size);

    let mut part_name = dest.as_os_str().to_owned();
    part_name.push(".part");
    let part = PathBuf::from(part_name);

    let result = (|| -> Result<(), OcrError> {
        let mut out = std::fs::File::create(&part)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut done = 0u64;
        progress(file.file_name, 0, total);
        loop {
            let n = resp
                .read(&mut buf)
                .map_err(|e| OcrError::Download(format!("{url}: {e}")))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            hasher.update(&buf[..n]);
            done += n as u64;
            progress(file.file_name, done, total);
        }
        out.flush()?;
        drop(out);
        let actual = const_hex::encode(hasher.finalize());
        if done != file.size {
            return Err(OcrError::Checksum {
                path: part.clone(),
                expected: format!("{} bytes", file.size),
                actual: format!("{done} bytes"),
            });
        }
        check_hash(&part, file, actual)
    })();

    match result {
        Ok(()) => {
            std::fs::rename(&part, dest)?;
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
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
}
