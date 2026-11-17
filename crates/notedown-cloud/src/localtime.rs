//! This computer's offset from UTC, for local days and dates shown to people.

use time::UtcOffset;

/// `+HH:MM`, `-HH:MM`, `+HHMM` or `Z`.
pub fn parse_utc_offset(text: &str) -> Option<UtcOffset> {
    let t = text.trim();
    if t == "Z" || t == "z" {
        return Some(UtcOffset::UTC);
    }
    let (sign, rest) = match t.as_bytes().first()? {
        b'+' => (1i8, &t[1..]),
        b'-' => (-1i8, &t[1..]),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|c| *c != ':').collect();
    if digits.len() != 4 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let h: i8 = digits[..2].parse().ok()?;
    let m: i8 = digits[2..].parse().ok()?;
    UtcOffset::from_hms(sign * h, sign * m, 0).ok()
}

/// This computer's current offset from UTC. The time crate cannot read it safely once threads
/// run; jiff reads `TZ` or the system zone data itself, without `localtime_r` and without a
/// child process (a forked child would hold every open file lock of the process until it execs).
pub fn local_utc_offset() -> Option<UtcOffset> {
    let zone = jiff::tz::TimeZone::try_system().ok()?;
    UtcOffset::from_whole_seconds(zone.to_offset(jiff::Timestamp::now()).seconds()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_offsets_parse_in_the_usual_forms() {
        let two = UtcOffset::from_hms(2, 0, 0).unwrap();
        assert_eq!(parse_utc_offset("+02:00"), Some(two));
        assert_eq!(parse_utc_offset("+0200\n"), Some(two));
        assert_eq!(
            parse_utc_offset("-05:30"),
            Some(UtcOffset::from_hms(-5, -30, 0).unwrap())
        );
        assert_eq!(parse_utc_offset("Z"), Some(UtcOffset::UTC));
        for bad in ["", "02:00", "+2", "+02:0x", "CEST"] {
            assert_eq!(parse_utc_offset(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_local_offset_is_read_and_whole_minutes() {
        let offset = local_utc_offset().expect("this computer has a time zone");
        assert_eq!(offset.whole_seconds() % 60, 0);
    }
}
