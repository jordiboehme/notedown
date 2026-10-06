//! Metadata block syntax: a concatenation of `<KEY:VALUE>` items
//! (docs/note-format.md 3).

use serde::Serialize;

/// The items of one metadata block, in file order.
///
/// Keys may repeat (docs/note-format.md 3), so this is an ordered list and not a map.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Meta {
    entries: Vec<(String, String)>,
}

impl Meta {
    /// Parses a metadata payload. The text is decoded as UTF-8 with replacement
    /// (docs/note-format.md 10.3). On error the message says what is wrong.
    pub fn parse(payload: &[u8]) -> Result<Meta, String> {
        let text = String::from_utf8_lossy(payload);
        let mut entries = Vec::new();
        let mut rest: &str = &text;
        while !rest.is_empty() {
            let offset = text.len() - rest.len();
            let Some(item_and_rest) = rest.strip_prefix('<') else {
                return Err(format!("text outside an item at offset {offset}"));
            };
            // docs/note-format.md 3: VALUE runs to the next '>'
            let Some((item, after)) = item_and_rest.split_once('>') else {
                return Err(format!("unterminated item at offset {offset}"));
            };
            let Some((key, value)) = item.split_once(':') else {
                return Err(format!("item without ':' at offset {offset}"));
            };
            // KEY is non-empty and holds no ':', '<' or '>'; values hold no '<'
            if key.is_empty() || key.contains('<') {
                return Err(format!("invalid key at offset {offset}"));
            }
            if value.contains('<') {
                return Err(format!("'<' inside the value of {key} at offset {offset}"));
            }
            entries.push((key.to_owned(), value.to_owned()));
            rest = after;
        }
        Ok(Meta { entries })
    }

    /// Builds a block from key/value pairs (for tests and tools).
    pub fn from_pairs<K: Into<String>, V: Into<String>>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Meta {
        Meta {
            entries: pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    /// The first value of `key`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// All values of `key`, in file order.
    pub fn get_all<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.entries
            .iter()
            .filter(move |(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// All items in file order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Number of items.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the block has no items.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::Meta;

    #[test]
    fn parses_items_in_order() {
        let meta = Meta::parse(b"<PAGE1:185284><DIRTY:1><FILE_FEATURE:3194>").unwrap();
        let items: Vec<_> = meta.iter().collect();
        assert_eq!(
            items,
            vec![
                ("PAGE1", "185284"),
                ("DIRTY", "1"),
                ("FILE_FEATURE", "3194")
            ]
        );
    }

    #[test]
    fn keeps_repeated_keys() {
        let meta = Meta::parse(b"<TITLE_0001:5><X:1><TITLE_0001:9>").unwrap();
        assert_eq!(meta.get("TITLE_0001"), Some("5"));
        assert_eq!(
            meta.get_all("TITLE_0001").collect::<Vec<_>>(),
            vec!["5", "9"]
        );
        assert_eq!(meta.len(), 3);
    }

    #[test]
    fn value_runs_to_the_next_bracket() {
        // docs/note-format.md 3: a value may contain ':'; it may be empty
        let meta = Meta::parse(b"<A:x:y:z><B:><C:a=b/c>").unwrap();
        assert_eq!(meta.get("A"), Some("x:y:z"));
        assert_eq!(meta.get("B"), Some(""));
        assert_eq!(meta.get("C"), Some("a=b/c"));
    }

    #[test]
    fn empty_payload_is_an_empty_block() {
        assert!(Meta::parse(b"").unwrap().is_empty());
    }

    #[test]
    fn invalid_utf8_is_replaced() {
        let meta = Meta::parse(b"<K:a\xffb>").unwrap();
        assert_eq!(meta.get("K"), Some("a\u{fffd}b"));
    }

    #[test]
    fn rejects_malformed_syntax() {
        for bad in [
            &b"junk"[..],
            b"<A:1>junk",
            b"<A:1",
            b"<A1>",
            b"<:1>",
            b"<A:1<B:2>",
            b"<A<B:2>",
            b"\x00\x01\x02\x03",
        ] {
            assert!(
                Meta::parse(bad).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }
}
