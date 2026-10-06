//! Typed reading of metadata values (docs/note-format.md 3, "Value types").
//!
//! Policy: values the parser dereferences (addresses) or needs for identity
//! (link type and rect, title and keyword rect) are strict and give
//! `NoteError::Corrupt` naming the key. Other typed values are lenient: a
//! malformed value becomes `None`, a warning is recorded, and the raw value stays
//! in the block's `Meta`.

use base64::Engine;

use crate::error::{NoteError, corrupt};
use crate::meta::Meta;
use crate::model::{KeyPosition, Timestamp};

/// Collects warnings about values that could not be typed.
#[derive(Debug, Default)]
pub(crate) struct Warnings(pub(crate) Vec<String>);

impl Warnings {
    pub(crate) fn push(&mut self, message: String) {
        self.0.push(message);
    }
}

/// `none` and the empty string mean "absent" (docs/note-format.md 3).
fn absent(value: &str) -> bool {
    value.is_empty() || value == "none"
}

/// A string value; `None` for a missing key, `none` or empty.
pub(crate) fn text(meta: &Meta, key: &str) -> Option<String> {
    meta.get(key).filter(|v| !absent(v)).map(str::to_owned)
}

/// Decimal `u32` with no sign and no overflow (docs/note-format.md 10.3).
pub(crate) fn parse_u32(value: &str) -> Option<u32> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

/// A lenient `u32` value.
pub(crate) fn uint(warnings: &mut Warnings, what: &str, meta: &Meta, key: &str) -> Option<u32> {
    let value = meta.get(key).filter(|v| !absent(v))?;
    let parsed = parse_u32(value);
    if parsed.is_none() {
        warnings.push(format!(
            "{what} {key}: {value:?} is not an unsigned integer"
        ));
    }
    parsed
}

/// An address value: `None` for `0`, missing or `none`; `Corrupt` when the value
/// is not a `u32` (docs/note-format.md 3, 10.2, 10.3).
pub(crate) fn address(what: &str, key: &str, value: &str) -> Result<Option<u64>, NoteError> {
    if absent(value) {
        return Ok(None);
    }
    match parse_u32(value) {
        Some(0) => Ok(None),
        Some(a) => Ok(Some(u64::from(a))),
        None => Err(corrupt(format!(
            "{what} {key}: {value:?} is not an address"
        ))),
    }
}

/// `left,top,width,height` (docs/note-format.md 3, 7).
pub(crate) fn parse_rect(value: &str) -> Option<[u32; 4]> {
    let mut parts = value.split(',').map(|p| parse_u32(p.trim()));
    let rect = [
        parts.next()??,
        parts.next()??,
        parts.next()??,
        parts.next()??,
    ];
    if parts.next().is_some() {
        return None;
    }
    Some(rect)
}

/// A lenient rect value.
pub(crate) fn rect(
    warnings: &mut Warnings,
    what: &str,
    meta: &Meta,
    key: &str,
) -> Option<[u32; 4]> {
    let value = meta.get(key).filter(|v| !absent(v))?;
    let parsed = parse_rect(value);
    if parsed.is_none() {
        warnings.push(format!("{what} {key}: {value:?} is not a rect"));
    }
    parsed
}

/// A required rect: `primary`, else `fallback`; `Corrupt` naming the key when
/// neither is a valid rect.
pub(crate) fn required_rect(
    warnings: &mut Warnings,
    what: &str,
    meta: &Meta,
    primary: &str,
    fallback: Option<&str>,
) -> Result<[u32; 4], NoteError> {
    if let Some(r) = rect(warnings, what, meta, primary) {
        return Ok(r);
    }
    if let Some(r) = fallback.and_then(|k| rect(warnings, what, meta, k)) {
        return Ok(r);
    }
    Err(corrupt(format!("{what}: no valid {primary}")))
}

/// `YYYYMMDDhhmmss` + 6 digits (docs/note-format.md 3.1).
pub(crate) fn parse_timestamp(digits: &str) -> Option<Timestamp> {
    if digits.len() != 20 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let num = |range: std::ops::Range<usize>| digits.get(range).and_then(|s| s.parse::<u32>().ok());
    let small = |range| num(range).and_then(|v| u8::try_from(v).ok());
    Some(Timestamp {
        year: u16::try_from(num(0..4)?).ok()?,
        month: small(4..6)?,
        day: small(6..8)?,
        hour: small(8..10)?,
        minute: small(10..12)?,
        second: small(12..14)?,
        microsecond: num(14..20)?,
    })
}

/// The creation time inside a file or page id: one letter + 20 digits + suffix
/// (docs/note-format.md 3.1).
pub(crate) fn id_timestamp(id: &str) -> Option<Timestamp> {
    parse_timestamp(id.get(1..21)?)
}

/// Position digits after the prefix of a `TITLE_`, `KEYWORD_` or `LINKO_` key:
/// groups of 4 digits, page first (docs/note-format.md 4.1).
pub(crate) fn key_position(digits: &str) -> Option<KeyPosition> {
    if digits.is_empty()
        || !digits.len().is_multiple_of(4)
        || !digits.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let groups: Vec<u32> = digits
        .as_bytes()
        .chunks(4)
        .filter_map(|c| std::str::from_utf8(c).ok()?.parse().ok())
        .collect();
    Some(KeyPosition {
        page: *groups.first()?,
        top: groups.get(1).copied(),
        left: groups.get(2).copied(),
        height: groups.get(3).copied(),
        width: groups.get(4).copied(),
    })
}

/// Base64 text to UTF-8 (lossy), accepting padded and unpadded forms
/// (docs/note-format.md 7.3 `LINKFILE`).
pub(crate) fn base64_text(value: &str) -> Option<String> {
    let engines = [
        base64::engine::general_purpose::STANDARD,
        base64::engine::general_purpose::STANDARD_NO_PAD,
        base64::engine::general_purpose::URL_SAFE,
        base64::engine::general_purpose::URL_SAFE_NO_PAD,
    ];
    engines
        .iter()
        .find_map(|e| e.decode(value.trim()).ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_are_checked() {
        assert_eq!(parse_u32("0"), Some(0));
        assert_eq!(parse_u32("4294967295"), Some(u32::MAX));
        assert_eq!(parse_u32("4294967296"), None);
        assert_eq!(parse_u32("-1"), None);
        assert_eq!(parse_u32("+1"), None);
        assert_eq!(parse_u32(""), None);
    }

    #[test]
    fn addresses() {
        assert_eq!(address("f", "K", "185284").unwrap(), Some(185284));
        assert_eq!(address("f", "K", "0").unwrap(), None);
        assert_eq!(address("f", "K", "none").unwrap(), None);
        let err = address("footer", "PAGE1", "12x").unwrap_err().to_string();
        assert!(err.contains("PAGE1"), "{err}");
        assert!(address("f", "K", "99999999999").is_err());
    }

    #[test]
    fn rects() {
        assert_eq!(parse_rect("812,246,537,84"), Some([812, 246, 537, 84]));
        assert_eq!(parse_rect("1,2,3"), None);
        assert_eq!(parse_rect("1,2,3,4,5"), None);
        assert_eq!(parse_rect("1,-2,3,4"), None);
    }

    #[test]
    fn timestamps_and_ids() {
        let t = id_timestamp("F20240708174639550429bHksqaP2wKQ8").unwrap();
        assert_eq!(t.to_string(), "2024-07-08 17:46:39.550429");
        assert_eq!(parse_timestamp("2024"), None);
        assert_eq!(id_timestamp("none"), None);
    }

    #[test]
    fn key_positions() {
        let p = key_position("00010246081200840537").unwrap();
        assert_eq!(
            (p.page, p.top, p.left, p.height, p.width),
            (1, Some(246), Some(812), Some(84), Some(537))
        );
        let p = key_position("00020500").unwrap();
        assert_eq!((p.page, p.top, p.left), (2, Some(500), None));
        assert_eq!(key_position("001"), None);
        assert_eq!(key_position("00a1"), None);
    }

    #[test]
    fn base64_paths() {
        assert_eq!(
            base64_text("aHR0cHM6Ly9leGFtcGxlLmNvbS8=").as_deref(),
            Some("https://example.com/")
        );
        assert_eq!(
            base64_text("aHR0cHM6Ly9leGFtcGxlLmNvbS8").as_deref(),
            Some("https://example.com/")
        );
        assert_eq!(base64_text("***"), None);
    }
}
