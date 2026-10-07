//! A small reader for the YAML that front matter uses: block and flow mappings and lists,
//! plain, single- and double-quoted scalars and the block scalars `|` and `>` (with
//! chomping and indentation indicators). It reads what notedown writes and the forms
//! other tools (Crystalline, any YAML library) write back.
//!
//! A value it does not understand (a tag, an anchor, an alias, a broken flow value) is
//! read as [`Node::Unknown`], never as a wrong string: the header stays readable, but a
//! key that holds such a value cannot be used.

/// A parsed value. Scalars stay text; the caller decides what a number or a date is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Node {
    Scalar(String),
    List(Vec<Node>),
    Map(Vec<(String, Node)>),
    /// A value this reader does not understand.
    Unknown,
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
    /// The comment lines right before the key, each ending with `\n`.
    pub lead: String,
    /// The key's lines as they were, each ending with `\n`.
    pub raw: String,
}

/// The top-level keys of a block and the comment lines after the last one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Block {
    pub entries: Vec<Entry>,
    pub trailing: String,
}

const SPACE: [char; 2] = [' ', '\t'];

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn is_blank(line: &str) -> bool {
    line.trim_matches(SPACE).is_empty()
}

fn is_comment(line: &str) -> bool {
    line.trim_start_matches(SPACE).starts_with('#')
}

fn is_list_item(trimmed: &str) -> bool {
    trimmed == "-" || trimmed.starts_with("- ") || trimmed.starts_with("-\t")
}

/// A value that continues on the lines below its key: nothing or only a comment.
fn opens_block(rest: &str) -> bool {
    rest.is_empty() || rest.starts_with('#')
}

/// Splits the lines of a front matter block (without the `---` lines) into top-level keys.
/// `None` when a line does not fit the shape of a mapping.
pub(crate) fn top_level(lines: &[&str]) -> Option<Block> {
    let mut entries = Vec::new();
    let mut pending = String::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if is_blank(line) {
            i += 1;
            continue;
        }
        if is_comment(line) {
            pending.push_str(line);
            pending.push('\n');
            i += 1;
            continue;
        }
        if line.starts_with(SPACE) || is_list_item(line) {
            return None;
        }
        let (key, rest) = split_key(line)?;
        let mut j = i + 1;
        while j < lines.len()
            && (lines[j].starts_with(SPACE)
                || is_blank(lines[j])
                || (opens_block(rest) && is_list_item(lines[j])))
        {
            j += 1;
        }
        let mut end = j;
        while end > i + 1 && is_blank(lines[end - 1]) {
            end -= 1;
        }
        let value = value_of(rest, &lines[i + 1..j], 0);
        let raw = lines[i..end].iter().map(|l| format!("{l}\n")).collect();
        entries.push(Entry {
            key,
            value,
            lead: std::mem::take(&mut pending),
            raw,
        });
        i = j;
    }
    Some(Block {
        entries,
        trailing: pending,
    })
}

/// The value of a key or list item: `inline` after the colon or dash, `below` the lines
/// that belong to it, `parent` the column of the key or dash.
fn value_of(inline: &str, below: &[&str], parent: usize) -> Node {
    let inline = inline.trim_matches(SPACE);
    if opens_block(inline) {
        return if below.iter().all(|l| is_blank(l) || is_comment(l)) {
            Node::Scalar(String::new())
        } else {
            block(below, parent)
        };
    }
    match inline.chars().next() {
        Some('|' | '>') => block_scalar(inline, below, parent),
        Some('"' | '\'' | '{' | '[') => {
            let mut joined = inline.to_string();
            for l in below {
                joined.push('\n');
                joined.push_str(l);
            }
            let chars: Vec<char> = joined.chars().collect();
            let mut pos = 0;
            let Some(node) = flow(&chars, &mut pos, false) else {
                return Node::Unknown;
            };
            skip_ws(&chars, &mut pos);
            if pos == chars.len() {
                node
            } else {
                Node::Unknown
            }
        }
        Some('!' | '&' | '*' | '%' | '@' | '`') => Node::Unknown,
        _ => {
            let mut lines = vec![inline];
            lines.extend_from_slice(below);
            plain_lines(&lines)
        }
    }
}

/// A plain scalar over one or more lines: lines folded with a space, an empty line gives
/// a line break, a ` #` comment ends the value.
fn plain_lines(lines: &[&str]) -> Node {
    let mut parts: Vec<&str> = Vec::new();
    let mut ended = false;
    for (n, line) in lines.iter().enumerate() {
        let line = line.trim_matches(SPACE);
        if ended {
            if !(line.is_empty() || line.starts_with('#')) {
                return Node::Unknown;
            }
            continue;
        }
        if n > 0 && line.starts_with('#') {
            ended = true;
            continue;
        }
        let text = match comment_start(line) {
            Some(i) => {
                ended = true;
                line[..i].trim_end_matches(SPACE)
            }
            None => line,
        };
        if (n == 0 && text.starts_with("- ")) || has_mapping_colon(text) {
            return Node::Unknown;
        }
        parts.push(text);
    }
    Node::Scalar(fold(&parts))
}

/// The byte index of a ` #` comment in a plain line.
fn comment_start(line: &str) -> Option<usize> {
    let b = line.as_bytes();
    (1..b.len()).find(|&i| b[i] == b'#' && matches!(b[i - 1], b' ' | b'\t'))
}

/// `: ` or a final `:` in plain text, which YAML reads as a mapping.
fn has_mapping_colon(text: &str) -> bool {
    text.ends_with(':') || text.contains(": ") || text.contains(":\t")
}

/// Joins trimmed lines the way YAML folds plain and quoted scalars.
fn fold(lines: &[&str]) -> String {
    let lines: Vec<&str> = lines.iter().map(|l| l.trim_matches(SPACE)).collect();
    let Some(last) = lines.iter().rposition(|l| !l.is_empty()) else {
        return String::new();
    };
    let first = lines.iter().position(|l| !l.is_empty()).unwrap_or(0);
    let mut out = String::new();
    let mut empties = 0;
    for (n, line) in lines[first..=last].iter().enumerate() {
        if line.is_empty() {
            empties += 1;
            continue;
        }
        if n > 0 {
            if empties == 0 {
                out.push(' ');
            } else {
                out.push_str(&"\n".repeat(empties));
            }
        }
        out.push_str(line);
        empties = 0;
    }
    out
}

/// A block list, a block mapping or a scalar on the lines below a key.
fn block(lines: &[&str], parent: usize) -> Node {
    let Some(start) = lines.iter().position(|l| !is_blank(l) && !is_comment(l)) else {
        return Node::Scalar(String::new());
    };
    let first = lines[start];
    let indent = indent_of(first);
    let trimmed = first.trim_start_matches(' ');
    if trimmed.starts_with('\t') {
        return Node::Unknown;
    }
    if is_list_item(trimmed) {
        block_list(&lines[start..], indent)
    } else if !trimmed.starts_with(['"', '\'', '{', '[']) && split_key(trimmed).is_some() {
        block_map(&lines[start..], indent)
    } else if indent > parent {
        value_of(trimmed, &lines[start + 1..], parent)
    } else {
        Node::Unknown
    }
}

fn block_list(lines: &[&str], indent: usize) -> Node {
    let mut items = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if is_blank(line) || is_comment(line) {
            i += 1;
            continue;
        }
        let trimmed = line.trim_start_matches(' ');
        if indent_of(line) != indent || !is_list_item(trimmed) {
            return Node::Unknown;
        }
        let content = trimmed[1..].trim_start_matches(SPACE);
        let col = indent + (trimmed.len() - content.len());
        let mut j = i + 1;
        while j < lines.len() && (indent_of(lines[j]) > indent || is_blank(lines[j])) {
            j += 1;
        }
        let below = &lines[i + 1..j];
        let item = if !content.starts_with(['"', '\'', '{', '[', '|', '>', '#'])
            && !is_list_item(content)
            && split_key(content).is_some()
        {
            let first = format!("{}{content}", " ".repeat(col));
            let mut sub: Vec<&str> = vec![first.as_str()];
            sub.extend_from_slice(below);
            block_map(&sub, col)
        } else if is_list_item(content) {
            Node::Unknown
        } else {
            value_of(content, below, indent)
        };
        items.push(item);
        i = j;
    }
    Node::List(items)
}

fn block_map(lines: &[&str], indent: usize) -> Node {
    let mut entries = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if is_blank(line) || is_comment(line) {
            i += 1;
            continue;
        }
        if indent_of(line) != indent {
            return Node::Unknown;
        }
        let Some((key, rest)) = split_key(line.trim_start_matches(' ')) else {
            return Node::Unknown;
        };
        let mut j = i + 1;
        while j < lines.len()
            && (indent_of(lines[j]) > indent
                || is_blank(lines[j])
                || (opens_block(rest)
                    && indent_of(lines[j]) == indent
                    && is_list_item(lines[j].trim_start_matches(' '))))
        {
            j += 1;
        }
        entries.push((key, value_of(rest, &lines[i + 1..j], indent)));
        i = j;
    }
    Node::Map(entries)
}

/// The indicators of a block scalar header.
struct BlockHeader {
    folded: bool,
    /// `-` (strip), `+` (keep) or none (clip).
    chomp: Option<char>,
    /// The indentation indicator, relative to the parent's column.
    width: Option<usize>,
}

/// `|` or `>` with an optional chomping indicator (`-`, `+`) and indentation indicator
/// (`1`-`9`) in either order, then an optional comment.
fn block_header(header: &str) -> Option<BlockHeader> {
    let mut chars = header.chars();
    let folded = match chars.next()? {
        '|' => false,
        '>' => true,
        _ => return None,
    };
    let rest: String = chars.collect();
    let (flags, comment) = match rest.find(SPACE) {
        Some(i) => (&rest[..i], rest[i..].trim_matches(SPACE)),
        None => (rest.as_str(), ""),
    };
    if !(comment.is_empty() || comment.starts_with('#')) {
        return None;
    }
    let (mut chomp, mut width) = (None, None);
    for c in flags.chars() {
        match c {
            '-' | '+' if chomp.is_none() => chomp = Some(c),
            '1'..='9' if width.is_none() => width = c.to_digit(10).map(|d| d as usize),
            _ => return None,
        }
    }
    Some(BlockHeader {
        folded,
        chomp,
        width,
    })
}

/// A literal (`|`) or folded (`>`) block scalar on the lines below its key.
fn block_scalar(header: &str, below: &[&str], parent: usize) -> Node {
    let Some(h) = block_header(header) else {
        return Node::Unknown;
    };
    let indent = match h.width {
        Some(w) => parent + w,
        None => match below.iter().find(|l| !l.chars().all(|c| c == ' ')) {
            Some(l) => indent_of(l),
            None => parent + 1,
        },
    };
    if indent <= parent && below.iter().any(|l| !is_blank(l)) {
        return Node::Unknown;
    }
    let mut lines: Vec<&str> = Vec::new();
    for l in below {
        if l.chars().all(|c| c == ' ') {
            lines.push(l.get(indent..).unwrap_or(""));
        } else if indent_of(l) >= indent {
            lines.push(&l[indent..]);
        } else {
            return Node::Unknown;
        }
    }
    let last = lines.iter().rposition(|l| !l.is_empty());
    let mut out = String::new();
    if let Some(last) = last {
        if h.folded {
            let mut empties = 0;
            let mut prev_more: Option<bool> = None;
            for line in &lines[..=last] {
                if line.is_empty() {
                    empties += 1;
                    continue;
                }
                let more = line.starts_with(SPACE);
                match prev_more {
                    None => out.push_str(&"\n".repeat(empties)),
                    Some(false) if !more && empties == 0 => out.push(' '),
                    Some(false) if !more => out.push_str(&"\n".repeat(empties)),
                    Some(_) => out.push_str(&"\n".repeat(empties + 1)),
                }
                out.push_str(line);
                prev_more = Some(more);
                empties = 0;
            }
        } else {
            out = lines[..=last].join("\n");
        }
    }
    let trailing = lines.len() - last.map_or(0, |l| l + 1);
    if h.chomp != Some('-') && last.is_some() {
        out.push('\n');
    }
    if h.chomp == Some('+') {
        out.push_str(&"\n".repeat(trailing));
    }
    Node::Scalar(out)
}

/// `key: rest` with a plain or quoted key; `rest` is trimmed of spaces and tabs. `None`
/// when the line is not a mapping entry.
fn split_key(line: &str) -> Option<(String, &str)> {
    let line = line.trim_end_matches(SPACE);
    if line.starts_with(['"', '\'']) {
        let chars: Vec<char> = line.chars().collect();
        let mut pos = 0;
        let key = quoted(&chars, &mut pos)?;
        let consumed: usize = chars[..pos].iter().map(|c| c.len_utf8()).sum();
        let after = line[consumed..].trim_start_matches(SPACE);
        let rest = after.strip_prefix(':')?;
        if !(rest.is_empty() || rest.starts_with(SPACE)) {
            return None;
        }
        return Some((key, rest.trim_matches(SPACE)));
    }
    if line.starts_with(['{', '[', '#', '?', '!', '&', '*', '|', '>']) || is_list_item(line) {
        return None;
    }
    let bytes = line.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'#' && i > 0 && matches!(bytes[i - 1], b' ' | b'\t') {
            return None;
        }
        if b == b':' && (i + 1 == bytes.len() || matches!(bytes[i + 1], b' ' | b'\t')) {
            let key = line[..i].trim_matches(SPACE);
            if key.is_empty() {
                return None;
            }
            return Some((key.to_string(), line[i + 1..].trim_matches(SPACE)));
        }
    }
    None
}

/// Skips spaces, tabs, line breaks and ` #` comments (to the end of their line).
fn skip_ws(chars: &[char], pos: &mut usize) {
    while *pos < chars.len() {
        let c = chars[*pos];
        if matches!(c, ' ' | '\t' | '\n') {
            *pos += 1;
        } else if c == '#' && (*pos == 0 || matches!(chars[*pos - 1], ' ' | '\t' | '\n')) {
            while *pos < chars.len() && chars[*pos] != '\n' {
                *pos += 1;
            }
        } else {
            break;
        }
    }
}

/// A plain scalar in a flow collection: up to `,`, `}`, `]`, a comment or (for a key) a
/// `:` that ends it. Lines are folded.
fn flow_plain(chars: &[char], pos: &mut usize, key: bool) -> String {
    let start = *pos;
    while *pos < chars.len() {
        let c = chars[*pos];
        let next = chars.get(*pos + 1).copied();
        if matches!(c, ',' | '}' | ']')
            || (c == '#' && *pos > start && matches!(chars[*pos - 1], ' ' | '\t' | '\n'))
            || (key
                && c == ':'
                && next.is_none_or(|n| matches!(n, ' ' | '\t' | '\n' | ',' | '}' | ']')))
        {
            break;
        }
        *pos += 1;
    }
    let text: String = chars[start..*pos].iter().collect();
    let lines: Vec<&str> = text.split('\n').collect();
    fold(&lines)
}

/// A value inside a flow collection, or a quoted / flow value at the start of a line.
/// `None` when it does not fit.
fn flow(chars: &[char], pos: &mut usize, in_list: bool) -> Option<Node> {
    skip_ws(chars, pos);
    match chars.get(*pos)? {
        '"' | '\'' => Some(Node::Scalar(quoted(chars, pos)?)),
        '!' | '&' | '*' | '|' | '>' | '%' | '@' | '`' | '?' => None,
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
                    ',' | '!' | '&' | '*' | '?' | '{' | '[' => return None,
                    _ => {}
                }
                let key = match chars.get(*pos)? {
                    '"' | '\'' => quoted(chars, pos)?,
                    _ => flow_plain(chars, pos, true),
                };
                skip_ws(chars, pos);
                let value = if chars.get(*pos) == Some(&':') {
                    *pos += 1;
                    skip_ws(chars, pos);
                    if matches!(chars.get(*pos)?, ',' | '}') {
                        Node::Scalar(String::new())
                    } else {
                        flow(chars, pos, false)?
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
                    ',' => return None,
                    _ => {}
                }
                items.push(flow(chars, pos, true)?);
                skip_ws(chars, pos);
                match chars.get(*pos)? {
                    ',' => *pos += 1,
                    ']' => {}
                    _ => return None,
                }
            }
        }
        _ => {
            let text = flow_plain(chars, pos, false);
            if in_list && has_mapping_colon(&text) {
                return None;
            }
            Some(Node::Scalar(text))
        }
    }
}

/// After a line break inside a quoted scalar: skips the break, the indentation and any
/// empty lines. Returns how many line breaks there were.
fn line_breaks(chars: &[char], pos: &mut usize) -> usize {
    let mut breaks = 0;
    while let Some(&c) = chars.get(*pos) {
        match c {
            '\n' => breaks += 1,
            ' ' | '\t' => {}
            _ => break,
        }
        *pos += 1;
    }
    breaks
}

/// A single- or double-quoted scalar starting at `pos`; `pos` ends after the closing
/// quote. Line breaks fold like YAML does.
fn quoted(chars: &[char], pos: &mut usize) -> Option<String> {
    let q = *chars.get(*pos)?;
    *pos += 1;
    let mut out = String::new();
    // the length of `out` up to the last escape, whose spaces are content
    let mut kept = 0;
    while let Some(&c) = chars.get(*pos) {
        if c == '\n' {
            while out.len() > kept && out.ends_with(SPACE) {
                out.pop();
            }
            let breaks = line_breaks(chars, pos);
            if breaks == 1 {
                out.push(' ');
            } else {
                out.push_str(&"\n".repeat(breaks - 1));
            }
            continue;
        }
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
                if e == '\n' {
                    // an escaped line break joins the lines without a space
                    let breaks = line_breaks(chars, pos);
                    out.push_str(&"\n".repeat(breaks - 1));
                    kept = out.len();
                    continue;
                }
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
                kept = out.len();
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
            .entries
            .into_iter()
            .map(|e| (e.key, e.value))
            .collect()
    }

    fn one(text: &str) -> Node {
        read(text).remove(0).1
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
    fn keeps_raw_text_comments_and_reads_escapes() {
        let lines = [
            "# about the note",
            "verified:",
            "  - { by: human:jordi, at: 2026-10-06T09:00:00+02:00 }",
            "",
            "title: \"a\\u2028b \\\"q\\\" \\\\\" # note",
            "plain: x # comment",
            "# the end",
        ];
        let b = top_level(&lines).unwrap();
        assert_eq!(b.entries[0].lead, "# about the note\n");
        assert_eq!(
            b.entries[0].raw,
            "verified:\n  - { by: human:jordi, at: 2026-10-06T09:00:00+02:00 }\n"
        );
        assert_eq!(b.entries[1].value, s("a\u{2028}b \"q\" \\"));
        assert_eq!(b.entries[2].value, s("x"));
        assert_eq!(b.trailing, "# the end\n");
    }

    #[test]
    fn reads_block_scalars() {
        assert_eq!(one("t: |\n  a\n  b\n"), s("a\nb\n"));
        assert_eq!(one("t: |-\n  a\n\n  b\n\n"), s("a\n\nb"));
        assert_eq!(one("t: |+\n  a\n\n\nu: x\n"), s("a\n\n\n"));
        assert_eq!(
            one("t: |2-\n    indented\n    more\n"),
            s("  indented\n  more")
        );
        assert_eq!(one("t: |-2\n    x\n"), s("  x"));
        assert_eq!(
            one("t: >\n  folded\n  line\n\n  next\n"),
            s("folded line\nnext\n")
        );
        assert_eq!(
            one("t: >-\n  a\n    more\n  b\n"),
            s("a\n  more\nb"),
            "more-indented lines keep their breaks"
        );
        assert_eq!(
            one("t: | # note\n  # not a comment\n"),
            s("# not a comment\n")
        );
        // in a list and in a mapping inside a list
        let list = one("k:\n- |-\n  a\n  b\n- page: 1\n  text: |-\n    c\n    d\n");
        let items = list.as_list().unwrap();
        assert_eq!(items[0], s("a\nb"));
        assert_eq!(items[1].get("text"), Some(&s("c\nd")));
        assert_eq!(items[1].get("page"), Some(&s("1")));
        let keyword = one("k:\n- |2-\n    x\n   y\n");
        assert_eq!(keyword.as_list().unwrap()[0], s("  x\n y"));
    }

    #[test]
    fn folds_multi_line_quoted_and_plain_values() {
        assert_eq!(one("t: 'a\n  b\n\n  c'\n"), s("a b\nc"));
        assert_eq!(one("t: \"a \\\n  b\"\n"), s("a b"));
        assert_eq!(one("t: \"a\\\n  b\"\n"), s("ab"));
        assert_eq!(one("t: a\n  b\n"), s("a b"));
    }

    #[test]
    fn plain_values_keep_unicode_spaces() {
        assert_eq!(one("t: \u{a0}x\u{a0} \n"), s("\u{a0}x\u{a0}"));
        assert_eq!(
            one("g: { a: \u{a0}x\u{a0} }\n").get("a"),
            Some(&s("\u{a0}x\u{a0}"))
        );
    }

    #[test]
    fn flow_maps_need_a_space_after_the_colon_and_skip_comments() {
        let g = one("generated: { by:notedown/1 }\n");
        assert_eq!(g.get("by"), None);
        assert_eq!(g.get("by:notedown/1"), Some(&s("")));
        let lines = ["generated: { x: a # , by: notedown/1.0.0", "  }"];
        let g = top_level(&lines).unwrap().entries.remove(0).value;
        assert_eq!(g, Node::Map(vec![("x".into(), s("a"))]));
        let lines = ["generated: { by: other, # x", "  at: y }"];
        let g = top_level(&lines).unwrap().entries.remove(0).value;
        assert_eq!(g.get("by"), Some(&s("other")));
        assert_eq!(g.get("at"), Some(&s("y")));
    }

    #[test]
    fn values_it_does_not_understand_are_unknown() {
        for text in [
            "t: !!str x\n",
            "t: &a x\n",
            "t: *a\n",
            "t: { a: b\n",
            "t: { by: !!str x }\n",
            "t: |x\n  a\n",
            "t: a: b\n",
            "t: \"bad \\q\"\n",
            "t: |2\n x\n",
        ] {
            assert_eq!(one(text), Node::Unknown, "{text:?}");
        }
        // the rest of the header stays readable
        let r = read("a: !tag x\nb: y\n");
        assert_eq!(r[1].1, s("y"));
    }

    #[test]
    fn refuses_what_does_not_fit() {
        for bad in [&["  indented: x"][..], &["- item"], &["no colon"]] {
            assert_eq!(top_level(bad), None, "{bad:?}");
        }
    }
}
