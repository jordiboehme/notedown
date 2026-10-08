//! Wire paths: the device's path strings, parsed into byte-exact segments.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// A path in the device tree, relative to `files/`. The device sends many forms
/// (`/NOTE/Note/x.note`, `NOTE/Note/x.note`, `NOTE/Note//x.note`, a trailing slash); all parse to
/// the same segments. Segments are kept byte-exact: no Unicode normalization, no case folding.
/// The root has no segments.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct WirePath {
    segments: Vec<String>,
}

fn invalid(message: String) -> Error {
    Error::Invalid {
        what: "path",
        message,
    }
}

fn check_segment(segment: &str) -> Result<()> {
    if segment.is_empty() {
        return Err(invalid("empty name".into()));
    }
    if segment == "." || segment == ".." {
        return Err(invalid(format!("{segment:?} is not allowed")));
    }
    if segment.contains('\0') {
        return Err(invalid("NUL in a name".into()));
    }
    if segment.contains('\\') {
        return Err(invalid("backslash in a name".into()));
    }
    if segment.contains('/') {
        return Err(invalid("slash in a name".into()));
    }
    Ok(())
}

impl WirePath {
    /// The user root (`/` on the wire).
    pub fn root() -> WirePath {
        WirePath::default()
    }

    /// Parse any device form. Empty segments (double, leading and trailing slashes) are dropped;
    /// `""` and `"/"` give the root. `.`, `..`, NUL and `\` are refused with `Error::Invalid`.
    pub fn parse(text: &str) -> Result<WirePath> {
        let mut segments = Vec::new();
        for segment in text.split('/').filter(|s| !s.is_empty()) {
            check_segment(segment)?;
            segments.push(segment.to_string());
        }
        Ok(WirePath { segments })
    }

    /// Like [`WirePath::parse`], but the path must name a file or folder: the root (an empty
    /// path after normalization) is refused with `Error::Invalid`.
    pub fn parse_named(text: &str) -> Result<WirePath> {
        let path = WirePath::parse(text)?;
        if path.is_root() {
            return Err(invalid(format!("{text:?} names no file or folder")));
        }
        Ok(path)
    }

    /// Build from segments, each checked like a parsed one.
    pub fn from_segments<I, S>(segments: I) -> Result<WirePath>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let segments: Vec<String> = segments.into_iter().map(Into::into).collect();
        for s in &segments {
            check_segment(s)?;
        }
        Ok(WirePath { segments })
    }

    pub fn is_root(&self) -> bool {
        self.segments.is_empty()
    }

    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// The last segment; `None` for the root.
    pub fn name(&self) -> Option<&str> {
        self.segments.last().map(String::as_str)
    }

    /// The parent path; `None` for the root.
    pub fn parent(&self) -> Option<WirePath> {
        let (_, parent) = self.segments.split_last()?;
        Some(WirePath {
            segments: parent.to_vec(),
        })
    }

    /// This path with one more segment. `name` must be a single valid segment.
    pub fn join(&self, name: &str) -> Result<WirePath> {
        check_segment(name)?;
        let mut segments = self.segments.clone();
        segments.push(name.to_string());
        Ok(WirePath { segments })
    }

    /// `path_display` form: `NOTE/Note/x.note`, no leading slash; `""` for the root.
    pub fn display(&self) -> String {
        self.segments.join("/")
    }

    /// `parent_path` form: `/NOTE/Note`, with a leading slash. `None` for the root and for
    /// top-level entries (their parent is the root, and listings omit the field).
    pub fn parent_path(&self) -> Option<String> {
        if self.segments.len() < 2 {
            return None;
        }
        Some(format!(
            "/{}",
            self.segments[..self.segments.len() - 1].join("/")
        ))
    }

    /// Segment-aware prefix test: `NOTE/Note/Work/x` starts with `NOTE/Note/Work`,
    /// `NOTE/Note/Workshop` does not. Every path starts with itself and with the root.
    pub fn starts_with(&self, prefix: &WirePath) -> bool {
        self.segments.starts_with(&prefix.segments)
    }

    /// Replace the prefix `from` with `to`; `None` if this path is not under `from`.
    pub fn rebase(&self, from: &WirePath, to: &WirePath) -> Option<WirePath> {
        if !self.starts_with(from) {
            return None;
        }
        let mut segments = to.segments.clone();
        segments.extend_from_slice(&self.segments[from.segments.len()..]);
        Some(WirePath { segments })
    }

    /// A range bound right after the subtree of this path's ancestor with `depth` segments (the
    /// path itself when `depth` is its length): that ancestor with a NUL after its last name. In
    /// the order of paths it comes after everything below the ancestor and before its next
    /// sibling, as no real name holds a NUL. Only a bound, never a real path. `depth` is 1 to
    /// the length of the path.
    pub(crate) fn subtree_end(&self, depth: usize) -> WirePath {
        let mut segments = self.segments[..depth].to_vec();
        if let Some(last) = segments.last_mut() {
            last.push('\0');
        }
        WirePath { segments }
    }

    /// True if any segment starts with `.`. Listings hide such paths.
    pub fn is_hidden(&self) -> bool {
        self.segments.iter().any(|s| s.starts_with('.'))
    }

    /// The location of this path under `base` (the account's `files/`).
    pub fn to_fs(&self, base: &Path) -> PathBuf {
        let mut path = base.to_path_buf();
        path.extend(&self.segments);
        path
    }
}

impl fmt::Display for WirePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}
