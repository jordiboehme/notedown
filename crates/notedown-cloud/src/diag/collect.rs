//! The window of a diagnose bundle and the readers of the protocol log and the journal. Lines
//! are read as JSON values, not as typed entries, so a line of an older or newer notedown is
//! read too.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::{Date, Duration, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

use crate::{Error, Result, io_err};

/// The time span of a bundle: `from` up to `to` (now).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub from: OffsetDateTime,
    pub to: OffsetDateTime,
}

impl Window {
    pub fn contains(&self, t: OffsetDateTime) -> bool {
        t >= self.from && t <= self.to
    }
}

/// `--since`: a date (`2026-10-09`, local midnight), an RFC 3339 time, or a duration back from
/// `now` (`30m`, `12h`, `3d`).
pub fn parse_since(text: &str, now: OffsetDateTime, local: UtcOffset) -> Result<OffsetDateTime> {
    let t = text.trim();
    let invalid = || Error::Invalid {
        what: "--since",
        message: format!(
            "{t:?} is no date (2026-10-09), time (2026-10-09T08:00:00Z) or duration (30m, 12h, 3d)"
        ),
    };
    if let Ok(at) = OffsetDateTime::parse(t, &Rfc3339) {
        return Ok(at);
    }
    let format = time::macros::format_description!("[year]-[month]-[day]");
    if let Ok(day) = Date::parse(t, &format) {
        return Ok(PrimitiveDateTime::new(day, Time::MIDNIGHT).assume_offset(local));
    }
    let unit = t.chars().last().ok_or_else(invalid)?;
    let n: i64 = t[..t.len() - unit.len_utf8()]
        .parse()
        .map_err(|_| invalid())?;
    let span = match unit {
        'm' => Duration::minutes(n),
        'h' => Duration::hours(n),
        'd' => Duration::days(n),
        _ => return Err(invalid()),
    };
    if n < 0 {
        return Err(invalid());
    }
    Ok(now - span)
}

/// The protocol log files whose UTC day overlaps the window, oldest first.
pub fn log_files(logs_dir: &Path, w: &Window) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(logs_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(logs_dir)(e)),
    };
    let first = w.from.to_offset(UtcOffset::UTC).date();
    let last = w.to.to_offset(UtcOffset::UTC).date();
    let format = time::macros::format_description!("[year]-[month]-[day]");
    let mut files: Vec<(Date, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let day = name
                .strip_prefix("protocol-")?
                .strip_suffix(".jsonl")
                .and_then(|d| Date::parse(d, &format).ok())?;
            (day >= first && day <= last).then(|| (day, e.path()))
        })
        .collect();
    files.sort();
    Ok(files.into_iter().map(|(_, p)| p).collect())
}

/// Call `each` with every complete line of `path` parsed as JSON. A last line without a
/// newline is being written: it is left out without a word. Returns how many lines could not
/// be read; one bad line never stops the reading.
pub fn read_lines(path: &Path, each: &mut dyn FnMut(Value) -> Result<()>) -> Result<u64> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(io_err(path)(e)),
    };
    let mut reader = BufReader::new(file);
    let mut bad = 0;
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line).map_err(io_err(path))?;
        if n == 0 || line.last() != Some(&b'\n') {
            return Ok(bad);
        }
        let text = &line[..line.len() - 1];
        if text.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Value>(text) {
            Ok(v) => each(v)?,
            Err(_) => bad += 1,
        }
    }
}

/// The `ts` of a protocol log line (RFC 3339).
pub fn log_ts(v: &Value) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(v.get("ts")?.as_str()?, &Rfc3339).ok()
}

/// The `ts` of a journal event (ms since the epoch).
pub fn journal_ts(v: &Value) -> Option<OffsetDateTime> {
    let ms = v.get("ts")?.as_u64()?;
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok()
}

/// A heartbeat frame: `2` in, `3` out, `ratta_ping` in and its `Received` out.
pub fn is_heartbeat(v: &Value) -> bool {
    if v.get("kind").and_then(Value::as_str) != Some("frame") {
        return false;
    }
    let body = |k: &str| v.get(k).and_then(Value::as_str);
    match v.get("direction").and_then(Value::as_str) {
        Some("in") => matches!(body("request_body"), Some("2" | r#"42["ratta_ping"]"#)),
        Some("out") => matches!(
            body("response_body"),
            Some("3" | r#"42["ratta_ping","Received"]"#)
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::macros::{datetime, offset};

    use super::*;

    const NOW: OffsetDateTime = datetime!(2026-10-09 14:10 UTC);

    #[test]
    fn since_parses_dates_times_and_durations() {
        assert_eq!(
            parse_since("3d", NOW, offset!(+2)).unwrap(),
            datetime!(2026-10-06 14:10 UTC)
        );
        assert_eq!(
            parse_since("12h", NOW, offset!(+2)).unwrap(),
            datetime!(2026-10-09 02:10 UTC)
        );
        assert_eq!(
            parse_since("30m", NOW, offset!(+2)).unwrap(),
            datetime!(2026-10-09 13:40 UTC)
        );
        assert_eq!(
            parse_since("2026-10-09", NOW, offset!(+2)).unwrap(),
            datetime!(2026-10-08 22:00 UTC)
        );
        assert_eq!(
            parse_since("2026-10-09T08:00:00Z", NOW, offset!(+2)).unwrap(),
            datetime!(2026-10-09 08:00 UTC)
        );
    }

    #[test]
    fn a_bad_since_is_an_error() {
        for bad in ["", "3w", "x", "-3d", "2026-13-01", "d"] {
            let err = parse_since(bad, NOW, UtcOffset::UTC).unwrap_err();
            assert!(err.to_string().contains("--since"), "{bad}: {err}");
        }
    }

    #[test]
    fn log_files_overlap_the_window() {
        let dir = tempfile::tempdir().unwrap();
        for d in ["2026-10-05", "2026-10-06", "2026-10-09", "2026-10-10"] {
            std::fs::write(dir.path().join(format!("protocol-{d}.jsonl")), "").unwrap();
        }
        std::fs::write(dir.path().join("other.txt"), "").unwrap();
        let w = Window {
            from: datetime!(2026-10-06 23:00 UTC),
            to: NOW,
        };
        let names: Vec<_> = log_files(dir.path(), &w)
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            ["protocol-2026-10-06.jsonl", "protocol-2026-10-09.jsonl"]
        );
        assert!(log_files(&dir.path().join("none"), &w).unwrap().is_empty());
    }

    #[test]
    fn a_torn_last_line_is_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.jsonl");
        std::fs::write(&p, "{\"a\":1}\n{\"a\":2}\n{\"a\":").unwrap();
        let mut seen = Vec::new();
        let bad = read_lines(&p, &mut |v| {
            seen.push(v);
            Ok(())
        })
        .unwrap();
        assert_eq!(bad, 0);
        assert_eq!(seen, [json!({"a": 1}), json!({"a": 2})]);
    }

    #[test]
    fn a_bad_line_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.jsonl");
        std::fs::write(&p, "{\"a\":1}\nnot json\n\n{\"a\":2}\n").unwrap();
        let mut n = 0;
        let bad = read_lines(&p, &mut |_| {
            n += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!((n, bad), (2, 1));
    }

    #[test]
    fn heartbeats_are_recognised() {
        let frame = |dir: &str, key: &str, body: &str| json!({"kind": "frame", "direction": dir, key: body});
        assert!(is_heartbeat(&frame("in", "request_body", "2")));
        assert!(is_heartbeat(&frame("out", "response_body", "3")));
        assert!(is_heartbeat(&frame(
            "in",
            "request_body",
            r#"42["ratta_ping"]"#
        )));
        assert!(is_heartbeat(&frame(
            "out",
            "response_body",
            r#"42["ratta_ping","Received"]"#
        )));
        assert!(!is_heartbeat(&frame(
            "in",
            "request_body",
            r#"421["ClientMessage","Received"]"#
        )));
        assert!(!is_heartbeat(
            &json!({"kind": "request", "request_body": "2"})
        ));
    }

    #[test]
    fn timestamps_are_read() {
        assert_eq!(
            log_ts(&json!({"ts": "2026-10-09T08:14:24.75749Z"}))
                .unwrap()
                .hour(),
            8
        );
        assert_eq!(
            journal_ts(&json!({"ts": 1_791_322_352_271u64}))
                .unwrap()
                .year(),
            2026
        );
    }
}
