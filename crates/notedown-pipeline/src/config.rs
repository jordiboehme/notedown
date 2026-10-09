//! The `[pipeline]` section of `<data>/server.toml`. The server ignores it.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::{Error, Result};

/// What `notedown pipeline` reads from `server.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineConfig {
    /// A page is read this long after its last change.
    pub ocr_settle: Duration,
    /// The OCR model is unloaded after this long without work.
    pub model_idle: Duration,
    /// Days to keep old text versions.
    pub text_keep_days: u32,
    /// Wildcard wire paths of the notes to ingest.
    pub ingest_folders: Vec<String>,
    /// Wildcard wire paths left out.
    pub ingest_exclude: Vec<String>,
    /// Only notes with this keyword on the device are ingested. `None`: off.
    pub ingest_keyword: Option<String>,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        PipelineConfig {
            ocr_settle: Duration::from_secs(10 * 60),
            model_idle: Duration::from_secs(15 * 60),
            text_keep_days: 90,
            ingest_folders: vec!["NOTE/Note/**".to_string()],
            ingest_exclude: Vec::new(),
            ingest_keyword: None,
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Raw {
    ocr_settle: Option<String>,
    model_idle: Option<String>,
    text_keep_days: Option<u32>,
    ingest_folders: Option<Vec<String>>,
    ingest_exclude: Option<Vec<String>>,
    ingest_keyword: Option<String>,
}

#[derive(Deserialize, Default)]
struct File {
    pipeline: Option<Raw>,
}

impl PipelineConfig {
    /// Read `<data>/server.toml`. A missing file or a missing `[pipeline]` gives the defaults.
    pub fn load(data: &Path) -> Result<PipelineConfig> {
        let path = data.join("server.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PipelineConfig::default());
            }
            Err(source) => return Err(Error::Io { path, source }),
        };
        // Other keys belong to the server, so only the [pipeline] table is checked here.
        let file: File = toml::from_str(&text).map_err(|e| Error::Config {
            path: path.clone(),
            message: e.to_string(),
        })?;
        let Some(raw) = file.pipeline else {
            return Ok(PipelineConfig::default());
        };
        let bad = |message: String| Error::Config {
            path: path.clone(),
            message,
        };
        let mut c = PipelineConfig::default();
        if let Some(v) = &raw.ocr_settle {
            c.ocr_settle = parse_duration(v).map_err(|m| bad(format!("ocr_settle: {m}")))?;
        }
        if let Some(v) = &raw.model_idle {
            c.model_idle = parse_duration(v).map_err(|m| bad(format!("model_idle: {m}")))?;
        }
        if let Some(v) = raw.text_keep_days {
            c.text_keep_days = v;
        }
        if let Some(v) = raw.ingest_folders {
            c.ingest_folders = v;
        }
        if let Some(v) = raw.ingest_exclude {
            c.ingest_exclude = v;
        }
        c.ingest_keyword = raw.ingest_keyword.filter(|k| !k.trim().is_empty());
        Ok(c)
    }
}

/// `"90s"`, `"10m"`, `"2h"` or `"1d"`.
pub fn parse_duration(text: &str) -> std::result::Result<Duration, String> {
    let t = text.trim();
    let bad =
        || format!("\"{text}\" is not a duration, use a number and s, m, h or d, like \"10m\"");
    let unit = t.chars().last().ok_or_else(bad)?;
    let factor: u64 = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        _ => return Err(bad()),
    };
    let digits = &t[..t.len() - 1];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let n: u64 = digits.parse().map_err(|_| bad())?;
    n.checked_mul(factor)
        .map(Duration::from_secs)
        .ok_or_else(bad)
}

/// Whether the wire `path` matches `pattern`: `*` stands for any text within one segment, `**`
/// for any number of segments (also none).
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').collect();
    let s: Vec<&str> = path.split('/').collect();
    match_segments(&p, &s)
}

fn match_segments(p: &[&str], s: &[&str]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((&"**", rest)) => (0..=s.len()).any(|i| match_segments(rest, &s[i..])),
        Some((seg, rest)) => match s.split_first() {
            Some((first, tail)) => segment_match(seg, first) && match_segments(rest, tail),
            None => false,
        },
    }
}

fn segment_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, text: &str) {
        std::fs::write(dir.join("server.toml"), text).unwrap();
    }

    #[test]
    fn pipeline_defaults() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            PipelineConfig::load(d.path()).unwrap(),
            PipelineConfig::default()
        );
        write(d.path(), "scan_interval = 60\n");
        let c = PipelineConfig::load(d.path()).unwrap();
        assert_eq!(c.ocr_settle, Duration::from_secs(600));
        assert_eq!(c.model_idle, Duration::from_secs(900));
        assert_eq!(c.text_keep_days, 90);
        assert_eq!(c.ingest_folders, vec!["NOTE/Note/**"]);
        assert!(c.ingest_exclude.is_empty());
        assert_eq!(c.ingest_keyword, None);
    }

    #[test]
    fn pipeline_values_are_read() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "scan_interval = 60\n[pipeline]\nocr_settle = \"90s\"\nmodel_idle = \"2h\"\n\
             text_keep_days = 7\ningest_folders = [\"A/**\", \"B/*.note\"]\n\
             ingest_exclude = [\"A/tmp/**\"]\ningest_keyword = \"ai\"\n",
        );
        let c = PipelineConfig::load(d.path()).unwrap();
        assert_eq!(c.ocr_settle, Duration::from_secs(90));
        assert_eq!(c.model_idle, Duration::from_secs(7200));
        assert_eq!(c.text_keep_days, 7);
        assert_eq!(c.ingest_folders, vec!["A/**", "B/*.note"]);
        assert_eq!(c.ingest_exclude, vec!["A/tmp/**"]);
        assert_eq!(c.ingest_keyword.as_deref(), Some("ai"));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86400));
    }

    #[test]
    fn a_bad_duration_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        for bad in ["10", "m", "10x", "-5m", "1.5h", ""] {
            write(d.path(), &format!("[pipeline]\nocr_settle = \"{bad}\"\n"));
            let e = PipelineConfig::load(d.path()).unwrap_err().to_string();
            assert!(e.contains("ocr_settle"), "{bad}: {e}");
        }
        write(d.path(), "[pipeline]\nmodel_idle = \"soon\"\n");
        let e = PipelineConfig::load(d.path()).unwrap_err().to_string();
        assert!(e.contains("model_idle"), "{e}");
    }

    #[test]
    fn glob_match_cases() {
        assert!(glob_match("NOTE/Note/**", "NOTE/Note/a.note"));
        assert!(glob_match("NOTE/Note/**", "NOTE/Note/x/y/a.note"));
        assert!(!glob_match("NOTE/Note/**", "DOCUMENT/a"));
        assert!(glob_match("NOTE/Note/*.note", "NOTE/Note/a.note"));
        assert!(!glob_match("NOTE/Note/*.note", "NOTE/Note/sub/a.note"));
        assert!(glob_match("NOTE/**/a.note", "NOTE/a.note"));
        assert!(glob_match("NOTE/**/a.note", "NOTE/x/y/a.note"));
        assert!(!glob_match("NOTE/Note/a", "NOTE/Note/a/b"));
    }
}
