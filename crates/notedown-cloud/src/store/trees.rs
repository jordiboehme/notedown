//! The device folder trees: `NOTE/Note`, `NOTE/MyStyle`, `DOCUMENT/Document`, `EXPORT`,
//! `SCREENSHOT` and `INBOX` ([`crate::import::device_folders`]). Only paths inside them are
//! written by `cloud put` and the REST writes; the trees themselves are never renamed or
//! deleted. Paths compare byte-exact, segment by segment.

use super::WirePath;

/// The roots of the trees.
fn roots() -> impl Iterator<Item = WirePath> {
    crate::import::device_folders()
        .map(|root| WirePath::parse_named(root).expect("a device folder is a wire path"))
}

/// The root of the tree that holds `path` (the root itself included); `None` outside the trees.
pub fn device_tree_of(path: &WirePath) -> Option<WirePath> {
    roots().find(|root| path.starts_with(root))
}

/// `path` is the root of a tree. `NOTE` and `DOCUMENT` are none.
pub fn is_tree_root(path: &WirePath) -> bool {
    roots().any(|root| root == *path)
}

/// `path` may be written: inside a tree, not its root, and no segment is a dot name.
pub fn writable(path: &WirePath) -> bool {
    !path.is_hidden() && device_tree_of(path).is_some_and(|root| root != *path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wp(s: &str) -> WirePath {
        WirePath::parse(s).unwrap()
    }

    #[test]
    fn writable_is_inside_a_tree_but_not_a_root() {
        for yes in [
            "NOTE/Note/a.note",
            "NOTE/Note/x",
            "NOTE/MyStyle/a.png",
            "DOCUMENT/Document/b/c.pdf",
            "EXPORT/a",
            "SCREENSHOT/s.png",
            "INBOX/i",
        ] {
            assert!(writable(&wp(yes)), "{yes}");
        }
        for no in [
            "NOTE/Note",
            "NOTE",
            "DOCUMENT",
            "",
            "NOTE/Other/a.note",
            ".hidden",
            "NOTE/Note/.x",
            "NOTE/Note/.x/y",
            "EXPORTS/a",
            "note/Note/a.note",
        ] {
            assert!(!writable(&wp(no)), "{no:?}");
        }
        assert_eq!(device_tree_of(&wp("EXPORT/a/b")), Some(wp("EXPORT")));
        assert_eq!(device_tree_of(&wp("NOTE/Note")), Some(wp("NOTE/Note")));
        assert_eq!(
            device_tree_of(&wp("DOCUMENT/Document/x")),
            Some(wp("DOCUMENT/Document"))
        );
        assert_eq!(device_tree_of(&wp("NOTE")), None);
        assert_eq!(device_tree_of(&wp("")), None);
        assert_eq!(device_tree_of(&wp("EXPORTS/a")), None);
        for root in crate::import::device_folders() {
            assert!(is_tree_root(&wp(root)), "{root}");
        }
        for no in ["NOTE", "DOCUMENT", "", "NOTE/Note/x", "EXPORT/a"] {
            assert!(!is_tree_root(&wp(no)), "{no:?}");
        }
    }
}
