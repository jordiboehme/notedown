//! A small reader for the YAML that front matter uses: block and flow mappings and lists,
//! plain, single- and double-quoted scalars. It reads what notedown writes and the forms
//! other tools (Crystalline, any YAML library) write back. Anchors, tags and multi-line
//! block scalars (`|`, `>`) are not supported; a value that uses them is read as text.

/// A parsed value. Scalars stay text; the caller decides what a number or a date is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Node {
    Scalar(String),
    List(Vec<Node>),
    Map(Vec<(String, Node)>),
}

impl Node {
    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Node::Scalar(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn get(&self, key: &str) -> Option<&Node> {
        match self {
            Node::Map(entries) => entries.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// A list; an empty scalar (`key:` with nothing after it) counts as an empty list.
    pub(crate) fn as_list(&self) -> Option<&[Node]> {
        match self {
            Node::List(items) => Some(items),
            Node::Scalar(s) if s.is_empty() => Some(&[]),
            _ => None,
        }
    }
}

/// One top-level key of a front matter block, with its text as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub key: String,
    pub value: Node,
    /// The key's lines as they were, each ending with `\n`.
    pub raw: String,
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn is_blank_or_comment(line: &str) -> bool {
    let t = line.trim_start();
    t.is_empty() || t.starts_with('#')
}

fn is_list_item(trimmed: &str) -> bool {
    trimmed == "-" || trimmed.starts_with("- ")
}

/// Splits the lines of a front matter block (without the `---` lines) into top-level keys.
/// `None` when a line does not fit.
pub(crate) fn top_level(lines: &[&str]) -> Option<Vec<Entry>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if is_blank_or_comment(line) {
            i += 1;
            continue;
        }
        if line.starts_with([' ', '\t']) || is_list_item(line) {
            return None;
        }
        let (key, rest) = split_key(line)?;
        let mut j = i + 1;
        while j < lines.len()
            && (lines[j].starts_with([' ', '\t'])
                || lines[j].trim().is_empty()
                || (rest.is_empty() && is_list_item(lines[j])))
        {
            j += 1;
        }
        let mut end = j;
        while end > i + 1 && lines[end - 1].trim().is_empty() {
            end -= 1;
        }
        let value = value_of(rest, &lines[i + 1..end])?;
        let raw = lines[i..end].iter().map(|l| format!("{l}\n")).collect();
        out.push(Entry { key, value, raw });
        i = j;
    }
    Some(out)
}

/// The value of a key: inline after the colon, or the indented lines below it.
fn value_of(inline: &str, below: &[&str]) -> Option<Node> {
    let below: Vec<&str> = below
        .iter()
        .copied()
        .filter(|l| !is_blank_or_comment(l))
        .collect();
    if inline.is_empty() {
        return if below.is_empty() {
            Some(Node::Scalar(String::new()))
        } else {
            block(&below)
        };
    }
    // a flow value or plain text that goes on over several lines
    let mut joined = inline.to_string();
    for l in below {
        joined.push(' ');
        joined.push_str(l.trim());
    }
    inline_value(&joined)
}

/// A list, a mapping or a scalar spread over indented lines (none blank).
fn block(lines: &[&str]) -> Option<Node> {
    let first = lines.first()?;
    let indent = indent_of(first);
    let trimmed = first.trim_start();
    if is_list_item(trimmed) {
        block_list(lines, indent)
    } else if split_key(trimmed).is_some() {
        block_map(lines, indent)
    } else {
        let text: Vec<&str> = lines.iter().map(|l| l.trim()).collect();
        inline_value(&text.join(" "))
    }
}

fn block_list(lines: &[&str], indent: usize) -> Option<Node> {
    let mut items = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        if indent_of(line) != indent || !is_list_item(trimmed) {
            return None;
        }
        let content = trimmed[1..].trim_start_matches(' ');
        let col = indent + (trimmed.len() - content.len());
        let mut j = i + 1;
        while j < lines.len() && indent_of(lines[j]) > indent {
            j += 1;
        }
        let item = if content.is_empty() {
            if j == i + 1 {
                Node::Scalar(String::new())
            } else {
                block(&lines[i + 1..j])?
            }
        } else {
            let first = format!("{}{content}", " ".repeat(col));
            let mut sub: Vec<&str> = vec![first.as_str()];
            sub.extend_from_slice(&lines[i + 1..j]);
            block(&sub)?
        };
        items.push(item);
        i = j;
    }
    Some(Node::List(items))
}

fn block_map(lines: &[&str], indent: usize) -> Option<Node> {
    let mut entries = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if indent_of(line) != indent {
            return None;
        }
        let (key, rest) = split_key(line.trim_start())?;
        let mut j = i + 1;
        while j < lines.len()
            && (indent_of(lines[j]) > indent
                || (rest.is_empty()
                    && indent_of(lines[j]) == indent
                    && is_list_item(lines[j].trim_start())))
        {
            j += 1;
        }
        entries.push((key, value_of(rest, &lines[i + 1..j])?));
        i = j;
    }
    Some(Node::Map(entries))
}

/// `key: rest` with a plain or quoted key; `rest` is trimmed. `None` when the line is
/// not a mapping entry.
fn split_key(line: &str) -> Option<(String, &str)> {
    let line = line.trim_end();
    if line.starts_with(['"', '\'']) {
        let chars: Vec<char> = line.chars().collect();
        let mut pos = 0;
        let key = quoted(&chars, &mut pos)?;
        let after: String = chars[pos..].iter().collect();
        let after = after.trim_start();
        let rest = after.strip_prefix(':')?;
        if !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
            return None;
        }
        let offset = line.len() - rest.len();
        return Some((key, line[offset..].trim()));
    }
    if line.starts_with(['{', '[', '#']) || is_list_item(line) {
        return None;
    }
    let bytes = line.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'#' && i > 0 && bytes[i - 1] == b' ' {
            return None;
        }
        if b == b':' && (i + 1 == bytes.len() || matches!(bytes[i + 1], b' ' | b'\t')) {
            let key = line[..i].trim();
            if key.is_empty() {
                return None;
            }
            return Some((key.to_string(), line[i + 1..].trim()));
        }
    }
    None
}

/// A value written on one line: quoted, flow or plain, with an optional ` # comment`.
pub(crate) fn inline_value(s: &str) -> Option<Node> {
    let s = s.trim();
    let chars: Vec<char> = s.chars().collect();
    match chars.first() {
        Some('"' | '\'' | '{' | '[') => {
            let mut pos = 0;
            let node = flow(&chars, &mut pos)?;
            let rest: String = chars[pos..].iter().collect();
            let rest = rest.trim();
            (rest.is_empty() || rest.starts_with('#')).then_some(node)
        }
        _ => {
            let text = match s.find(" #") {
                Some(i) => &s[..i],
                None => s,
            };
            Some(Node::Scalar(text.trim().to_string()))
        }
    }
}

fn skip_ws(chars: &[char], pos: &mut usize) {
    while *pos < chars.len() && chars[*pos].is_whitespace() {
        *pos += 1;
    }
}

/// A value inside flow context, or a quoted / flow value at the start of a line.
fn flow(chars: &[char], pos: &mut usize) -> Option<Node> {
    skip_ws(chars, pos);
    match chars.get(*pos)? {
        '"' | '\'' => Some(Node::Scalar(quoted(chars, pos)?)),
        '{' => {
            *pos += 1;
            let mut entries = Vec::new();
            loop {
                skip_ws(chars, pos);
                match chars.get(*pos)? {
                    '}' => {
                        *pos += 1;
                        return Some(Node::Map(entries));
                    }
                    ',' => {
                        *pos += 1;
                        continue;
                    }
                    _ => {}
                }
                let key = match chars.get(*pos)? {
                    '"' | '\'' => quoted(chars, pos)?,
                    _ => {
                        let start = *pos;
                        while *pos < chars.len() && !matches!(chars[*pos], ':' | ',' | '}') {
                            *pos += 1;
                        }
                        chars[start..*pos]
                            .iter()
                            .collect::<String>()
                            .trim()
                            .to_string()
                    }
                };
                skip_ws(chars, pos);
                let value = if chars.get(*pos) == Some(&':') {
                    *pos += 1;
                    skip_ws(chars, pos);
                    if matches!(chars.get(*pos)?, ',' | '}') {
                        Node::Scalar(String::new())
                    } else {
                        flow(chars, pos)?
                    }
                } else {
                    Node::Scalar(String::new())
                };
                entries.push((key, value));
                skip_ws(chars, pos);
                match chars.get(*pos)? {
                    ',' => *pos += 1,
                    '}' => {}
                    _ => return None,
                }
            }
        }
        '[' => {
            *pos += 1;
            let mut items = Vec::new();
            loop {
                skip_ws(chars, pos);
                match chars.get(*pos)? {
                    ']' => {
                        *pos += 1;
                        return Some(Node::List(items));
                    }
                    ',' => {
                        *pos += 1;
                        continue;
                    }
                    _ => {}
                }
                items.push(flow(chars, pos)?);
                skip_ws(chars, pos);
                match chars.get(*pos)? {
                    ',' => *pos += 1,
                    ']' => {}
                    _ => return None,
                }
            }
        }
        _ => {
            let start = *pos;
            while *pos < chars.len() && !matches!(chars[*pos], ',' | '}' | ']') {
                *pos += 1;
            }
            let text: String = chars[start..*pos].iter().collect();
            Some(Node::Scalar(text.trim().to_string()))
        }
    }
}

/// A single- or double-quoted scalar starting at `pos`; `pos` ends after the closing quote.
fn quoted(chars: &[char], pos: &mut usize) -> Option<String> {
    let q = *chars.get(*pos)?;
    *pos += 1;
    let mut out = String::new();
    while let Some(&c) = chars.get(*pos) {
        *pos += 1;
        if q == '\'' {
            if c == '\'' {
                if chars.get(*pos) == Some(&'\'') {
                    out.push('\'');
                    *pos += 1;
                    continue;
                }
                return Some(out);
            }
            out.push(c);
            continue;
        }
        match c {
            '"' => return Some(out),
            '\\' => {
                let e = *chars.get(*pos)?;
                *pos += 1;
                let mut hex = |n: usize| -> Option<char> {
                    let h: String = chars.get(*pos..*pos + n)?.iter().collect();
                    *pos += n;
                    char::from_u32(u32::from_str_radix(&h, 16).ok()?)
                };
                out.push(match e {
                    '"' => '"',
                    '\\' => '\\',
                    '/' => '/',
                    ' ' => ' ',
                    '\t' => '\t',
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    '0' => '\0',
                    'a' => '\u{7}',
                    'b' => '\u{8}',
                    'e' => '\u{1b}',
                    'f' => '\u{c}',
                    'v' => '\u{b}',
                    'N' => '\u{85}',
                    '_' => '\u{a0}',
                    'L' => '\u{2028}',
                    'P' => '\u{2029}',
                    'x' => hex(2)?,
                    'u' => hex(4)?,
                    'U' => hex(8)?,
                    _ => return None,
                });
            }
            c => out.push(c),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Node {
        Node::Scalar(v.into())
    }

    fn read(text: &str) -> Vec<(String, Node)> {
        let lines: Vec<&str> = text.lines().collect();
        top_level(&lines)
            .unwrap()
            .into_iter()
            .map(|e| (e.key, e.value))
            .collect()
    }

    #[test]
    fn reads_flow_and_block_forms_alike() {
        let flow = read(
            "tags: [supernote]\n\
             generated: { by: notedown/0.1.0, model: \"a: b, c\", at: 2026-10-05T23:10:00Z }\n\
             links:\n  - { page: 1, target_md: 'it''s.md' }\n",
        );
        let block = read(
            "tags:\n- supernote\n\
             generated:\n  by: notedown/0.1.0\n  model: \"a: b, c\"\n  at: 2026-10-05T23:10:00Z\n\
             links:\n- page: 1\n  target_md: 'it''s.md'\n",
        );
        assert_eq!(flow, block);
        assert_eq!(flow[0].1, Node::List(vec![s("supernote")]));
        assert_eq!(flow[1].1.get("model"), Some(&s("a: b, c")));
        assert_eq!(
            flow[2].1.as_list().unwrap()[0].get("target_md"),
            Some(&s("it's.md"))
        );
    }

    #[test]
    fn keeps_raw_text_and_reads_escapes() {
        let lines = [
            "verified:",
            "  - { by: human:jordi, at: 2026-10-06T09:00:00+02:00 }",
            "",
            "title: \"a\\u2028b \\\"q\\\" \\\\\" # note",
            "plain: x # comment",
        ];
        let e = top_level(&lines).unwrap();
        assert_eq!(
            e[0].raw,
            "verified:\n  - { by: human:jordi, at: 2026-10-06T09:00:00+02:00 }\n"
        );
        assert_eq!(e[1].value, s("a\u{2028}b \"q\" \\"));
        assert_eq!(e[2].value, s("x"));
    }

    #[test]
    fn refuses_what_does_not_fit() {
        for bad in [
            &["  indented: x"][..],
            &["- item"],
            &["no colon"],
            &["k: {a: b"],
        ] {
            assert_eq!(top_level(bad), None, "{bad:?}");
        }
    }
}
