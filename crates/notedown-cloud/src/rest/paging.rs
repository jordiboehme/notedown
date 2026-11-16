//! Cursors and limits of the paged list routes (`GET /tree`, `GET /recycle`).
//!
//! A cursor names the last item of a page: `<kind><anchor in base64url>`, the kind with its
//! version (`t1:` for the tree, `r1:` for the recycle bin). Clients get it from `next` and send
//! it back unchanged; they never build one. A cursor of another kind or version, one that does
//! not decode and one longer than [`MAX_CURSOR`] is [`BadCursor`], which the routes answer with
//! 400 `bad_cursor` ([`bad_cursor`]). What the anchor must be is the route's business.

use axum::http::StatusCode;
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use super::routes::{Refusal, error, number};

/// The longest cursor that is decoded. A wire path below `PATH_MAX` gives a shorter one.
pub const MAX_CURSOR: usize = 8192;

/// The route a cursor belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorKind {
    /// `GET /tree`: the anchor is a wire path.
    Tree,
    /// `GET /recycle`: the anchor is an item key.
    Recycle,
}

impl CursorKind {
    fn prefix(self) -> &'static str {
        match self {
            CursorKind::Tree => "t1:",
            CursorKind::Recycle => "r1:",
        }
    }
}

/// A cursor that is not one of this kind: 400 `bad_cursor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadCursor;

/// The cursor of `kind` for `anchor`.
pub fn encode(kind: CursorKind, anchor: &str) -> String {
    format!("{}{}", kind.prefix(), URL_SAFE_NO_PAD.encode(anchor))
}

/// The anchor of a cursor of `kind`.
pub fn decode(kind: CursorKind, cursor: &str) -> Result<String, BadCursor> {
    if cursor.len() > MAX_CURSOR {
        return Err(BadCursor);
    }
    let text = cursor.strip_prefix(kind.prefix()).ok_or(BadCursor)?;
    let bytes = URL_SAFE_NO_PAD.decode(text).map_err(|_| BadCursor)?;
    String::from_utf8(bytes).map_err(|_| BadCursor)
}

/// 400 `bad_cursor`.
pub(super) fn bad_cursor() -> Response {
    error(StatusCode::BAD_REQUEST, "bad_cursor")
}

/// A `limit` query value: `default` when absent, at most `max`, 0 is 1. A value that is no
/// number is a 400 `bad_request`.
pub(super) fn limit(text: Option<&str>, default: usize, max: usize) -> Result<usize, Refusal> {
    let n = number(text, "limit", default as u64)?;
    Ok(usize::try_from(n).map_or(max, |n| n.clamp(1, max)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_gives_back_its_anchor() {
        for anchor in [
            "NOTE/Note/a b.note",
            "ä/x",
            "",
            "1763290000000/NOTE/Note/b.note",
        ] {
            for kind in [CursorKind::Tree, CursorKind::Recycle] {
                let c = encode(kind, anchor);
                assert!(c.is_ascii() && !c.contains(['+', '/', '=']), "{c}");
                assert_eq!(decode(kind, &c).as_deref(), Ok(anchor));
            }
        }
        assert!(encode(CursorKind::Tree, "x").starts_with("t1:"));
        assert!(encode(CursorKind::Recycle, "x").starts_with("r1:"));
    }

    #[test]
    fn a_cursor_of_another_kind_or_garbled_is_bad() {
        let tree = encode(CursorKind::Tree, "NOTE/Note/a");
        assert_eq!(decode(CursorKind::Recycle, &tree), Err(BadCursor));
        for c in ["", "t1", "t2:Tk9URQ", "t1:!!", "t1:Tk9URQ=", "Tk9URQ"] {
            assert_eq!(decode(CursorKind::Tree, c), Err(BadCursor), "{c}");
        }
        let not_utf8 = format!("t1:{}", URL_SAFE_NO_PAD.encode([0xff, 0xfe]));
        assert_eq!(decode(CursorKind::Tree, &not_utf8), Err(BadCursor));
        let long = encode(CursorKind::Tree, &"a/".repeat(MAX_CURSOR));
        assert_eq!(decode(CursorKind::Tree, &long), Err(BadCursor));
    }
}
