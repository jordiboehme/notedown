//! The OCR pause: how long it lasts, whether it holds now, and the line that shows it. The pause
//! lives in the config (`ocr_pause`), so it survives a restart of the app; a timed pause keeps
//! its end and ends at that time. Pure: the time comes in as `now_ms`.

use serde::{Deserialize, Serialize};

/// An OCR pause. `until_ms` is its end in ms since 1970 (this Mac's clock); `None` holds until
/// the user resumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrPause {
    pub until_ms: Option<u64>,
}

/// What the user picked in the Pause OCR menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PauseChoice {
    OneMinute,
    FiveMinutes,
    OneHour,
    UntilResumed,
}

const MINUTE_MS: u64 = 60_000;

/// The pause that `choice` starts at `now_ms`.
pub fn pause_for(choice: PauseChoice, now_ms: u64) -> OcrPause {
    let minutes = match choice {
        PauseChoice::OneMinute => 1,
        PauseChoice::FiveMinutes => 5,
        PauseChoice::OneHour => 60,
        PauseChoice::UntilResumed => return OcrPause { until_ms: None },
    };
    OcrPause {
        until_ms: Some(now_ms.saturating_add(minutes * MINUTE_MS)),
    }
}

/// Whether `p` holds at `now_ms`: until resumed, or its end is still ahead.
pub fn active(p: Option<&OcrPause>, now_ms: u64) -> bool {
    p.is_some_and(|p| p.until_ms.is_none_or(|until| until > now_ms))
}

/// Whether `p` is a timed pause whose end came: it is to be cleared.
pub fn expired(p: Option<&OcrPause>, now_ms: u64) -> bool {
    p.and_then(|p| p.until_ms)
        .is_some_and(|until| until <= now_ms)
}

/// A 24 hour clock time: "14:32", "09:05".
pub fn clock_text(hour: u32, minute: u32) -> String {
    format!("{hour:02}:{minute:02}")
}

/// The local clock time of `ms` (ms since 1970), as [`clock_text`] writes it.
pub fn local_clock(ms: u64) -> String {
    let secs = libc::time_t::try_from(ms / 1000).unwrap_or(libc::time_t::MAX);
    // SAFETY: `localtime_r` only reads `secs` and writes the zeroed `tm` it is given.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let local = unsafe { libc::localtime_r(&secs, &mut tm) };
    if local.is_null() {
        // No local time known: the time in UTC.
        let minutes = ms / MINUTE_MS;
        return clock_text(((minutes / 60) % 24) as u32, (minutes % 60) as u32);
    }
    clock_text(tm.tm_hour as u32, tm.tm_min as u32)
}

/// The line of a pause: "OCR paused until 14:32", or "OCR paused" until resumed. `clock` gives
/// the clock time of the end ([`local_clock`] in the app).
pub fn pause_line(p: &OcrPause, clock: impl Fn(u64) -> String) -> String {
    match p.until_ms {
        Some(until) => format!("OCR paused until {}", clock(until)),
        None => "OCR paused".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_763_300_000_000;

    #[test]
    fn each_choice_gives_its_end() {
        let until = |choice| pause_for(choice, NOW).until_ms;
        assert_eq!(until(PauseChoice::OneMinute), Some(NOW + 60_000));
        assert_eq!(until(PauseChoice::FiveMinutes), Some(NOW + 300_000));
        assert_eq!(until(PauseChoice::OneHour), Some(NOW + 3_600_000));
        assert_eq!(until(PauseChoice::UntilResumed), None);
    }

    #[test]
    fn a_pause_holds_until_its_end_and_expires_there() {
        let timed = OcrPause {
            until_ms: Some(NOW),
        };
        assert!(active(Some(&timed), NOW - 1));
        assert!(!expired(Some(&timed), NOW - 1));
        // At the end: over.
        assert!(!active(Some(&timed), NOW));
        assert!(expired(Some(&timed), NOW));
        assert!(!active(Some(&timed), NOW + 1));
        assert!(expired(Some(&timed), NOW + 1));
        // Until resumed: never over by itself.
        let open = OcrPause { until_ms: None };
        assert!(active(Some(&open), u64::MAX));
        assert!(!expired(Some(&open), u64::MAX));
        // No pause.
        assert!(!active(None, NOW));
        assert!(!expired(None, NOW));
    }

    #[test]
    fn the_clock_has_two_digits_each() {
        assert_eq!(clock_text(9, 5), "09:05");
        assert_eq!(clock_text(14, 32), "14:32");
        assert_eq!(clock_text(0, 0), "00:00");
    }

    #[test]
    fn the_local_clock_is_a_clock_time() {
        let text = local_clock(NOW);
        let (hour, minute) = text.split_once(':').unwrap();
        assert_eq!((hour.len(), minute.len()), (2, 2), "{text}");
        assert!(hour.parse::<u32>().unwrap() < 24, "{text}");
        assert!(minute.parse::<u32>().unwrap() < 60, "{text}");
        // NOW is 13:33:20 UTC. Time zones are whole hours (33), half hours (03) or, as in
        // Nepal, 45 minutes (18) off.
        assert!(["33", "03", "18"].contains(&minute), "{text}");
    }

    #[test]
    fn the_pause_line_names_the_end() {
        let timed = OcrPause {
            until_ms: Some(NOW),
        };
        let clock = |ms: u64| {
            assert_eq!(ms, NOW);
            "14:32".to_owned()
        };
        assert_eq!(pause_line(&timed, clock), "OCR paused until 14:32");
        let open = OcrPause { until_ms: None };
        assert_eq!(pause_line(&open, |_| unreachable!()), "OCR paused");
    }

    #[test]
    fn the_choice_and_the_pause_have_their_webview_names() {
        let choice: PauseChoice = serde_json::from_str(r#""untilResumed""#).unwrap();
        assert_eq!(choice, PauseChoice::UntilResumed);
        let choice: PauseChoice = serde_json::from_str(r#""fiveMinutes""#).unwrap();
        assert_eq!(choice, PauseChoice::FiveMinutes);
        let text = serde_json::to_string(&OcrPause {
            until_ms: Some(NOW),
        })
        .unwrap();
        assert_eq!(text, r#"{"untilMs":1763300000000}"#);
    }
}
