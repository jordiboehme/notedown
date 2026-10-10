//! The answer formats (spec 6.3): TOON for lists, a TOON head plus a Markdown body for a
//! delta, the OKF document for a note, JSON for the CLI.
//!
//! The tools build plain values; the format is chosen here, at the edge. Multi-line text never
//! goes into a TOON cell: the delta keeps small fields in tables and the texts as Markdown
//! below them.

use std::fmt::Write as _;

use notedown_convert::document::{
    Frontmatter, FrontmatterLink, HeaderFormat, PageOut, render_document,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::delta::{BlockChange, Delta, Manifest, NoteDelta, PageDelta};
use crate::model::{Change, NoteState, PageOcrState};
use crate::toon;

/// What the head of a delta tells about its words (spec 5.1).
const LEGEND: &str = "textBefore is the text when you last confirmed (or at your since); \
wordDiff: [-...-] removed, [+...+] added";

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The changed pages of a note: not those listed only because their OCR failed.
fn changed_pages(n: &NoteDelta) -> usize {
    n.pages.iter().filter(|p| !p.only_failed()).count()
}

/// "4 pages changed in 2 notes, 1 note pending, 1 page unreadable".
fn summary(delta: &Delta) -> String {
    let pages: usize = delta.notes.iter().map(changed_pages).sum();
    let changed = delta
        .notes
        .iter()
        .filter(|n| {
            let listed_for_state = n.pending || n.pages.iter().any(|p| p.failed);
            !(listed_for_state && changed_pages(n) == 0)
        })
        .count();
    let pending = delta.notes.iter().filter(|n| n.pending).count();
    let unreadable: usize = delta
        .notes
        .iter()
        .map(|n| n.pages.iter().filter(|p| p.failed).count())
        .sum();
    let mut out = if pages == 0 && changed == 0 {
        "no page changed".to_string()
    } else {
        format!(
            "{} changed in {}",
            plural(pages, "page", "pages"),
            plural(changed, "note", "notes")
        )
    };
    if pending > 0 {
        let _ = write!(out, ", {} pending", plural(pending, "note", "notes"));
    }
    if unreadable > 0 {
        let _ = write!(out, ", {} unreadable", plural(unreadable, "page", "pages"));
    }
    out
}

/// The name of a note in the body headings and the blocks table: its folder and title
/// (`Work/Planview`), so two notes with one title are told apart.
fn note_label(n: &NoteDelta) -> String {
    match n.path.rsplit_once('/') {
        Some((folder, _)) if !folder.is_empty() => format!("{folder}/{}", n.title),
        _ => n.title.clone(),
    }
}

fn line(key: &str, value: Value) -> String {
    toon::render(&json!({ key: value }))
}

fn null_if_empty(s: String) -> Value {
    if s.is_empty() {
        Value::Null
    } else {
        Value::String(s)
    }
}

fn note_row(n: &NoteDelta) -> Vec<Value> {
    let links: Vec<String> = n
        .links
        .iter()
        .map(|l| format!("p{} {}", l.page + 1, l.key.as_deref().unwrap_or(&l.target)))
        .collect();
    vec![
        json!(n.key),
        json!(n.title),
        json!(n.path),
        json!(n.change),
        json!(n.pending),
        json!(n.old_note),
        json!(n.old_path),
        null_if_empty(n.keywords.join(", ")),
        null_if_empty(links.join("; ")),
    ]
}

fn change_word(change: Change) -> String {
    serde_json::to_value(change)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// `b3` of `notedown://slug/key/page#b3`.
fn block_id(anchor: &str) -> &str {
    anchor.rsplit_once('#').map_or("", |(_, id)| id)
}

/// The delta as a TOON head plus a Markdown body (spec 6.3).
pub fn delta_text(delta: &Delta) -> String {
    let mut out = String::new();
    let range = format!("{}: {}", delta.range_text, summary(delta));
    out.push_str(&line("range", range.into()));
    out.push('\n');
    out.push_str(&line("from", delta.from.into()));
    out.push('\n');
    out.push_str(&line("upTo", delta.up_to.into()));
    out.push('\n');
    out.push_str(&line("more", delta.more.into()));
    out.push('\n');
    out.push_str(&line("legend", LEGEND.into()));
    out.push('\n');
    if !delta.notes.is_empty() {
        let rows: Vec<Vec<Value>> = delta.notes.iter().map(note_row).collect();
        let columns = [
            "key", "title", "path", "change", "pending", "oldNote", "oldPath", "keywords", "links",
        ];
        out.push_str(&toon::table("notes", &columns, &rows));
        out.push('\n');
    }
    let mut blocks = Vec::new();
    for n in &delta.notes {
        for p in &n.pages {
            for c in &p.what_changed {
                let replaced = c.replaced_by.join(" ");
                blocks.push(vec![
                    json!(note_label(n)),
                    json!(p.page),
                    json!(c.block),
                    json!(c.change),
                    null_if_empty(replaced),
                ]);
            }
        }
    }
    if !blocks.is_empty() {
        let columns = ["note", "page", "block", "change", "replacedBy"];
        out.push_str(&toon::table("blocks", &columns, &blocks));
        out.push('\n');
    }
    for n in &delta.notes {
        note_body(&mut out, delta, n);
    }
    out
}

fn note_body(out: &mut String, delta: &Delta, n: &NoteDelta) {
    if n.pages.is_empty() {
        let mut what = change_word(n.change);
        if n.pending {
            what.push_str(", pending");
        }
        let _ = writeln!(out, "\n## {} ({what})", note_label(n));
        return;
    }
    for p in &n.pages {
        page_body(out, delta, n, p);
    }
}

fn page_body(out: &mut String, delta: &Delta, n: &NoteDelta, p: &PageDelta) {
    let mut what = change_word(p.change);
    if p.failed {
        what.push_str(", unreadable");
    }
    if !p.signal.is_empty() {
        what.push_str(&format!("; {}", p.signal.join(", ")));
    }
    let _ = writeln!(out, "\n## {}, page {} ({what})", note_label(n), p.page);
    if !p.text_now.is_empty() {
        out.push_str("### textNow\n");
        let blocks: Vec<String> = p
            .text_now
            .iter()
            .map(|b| {
                format!(
                    "<!-- notedown://{}/{}/{}#{} -->\n{}\n",
                    delta.slug,
                    n.key,
                    p.page_id,
                    b.id,
                    b.text.trim_end()
                )
            })
            .collect();
        out.push_str(&blocks.join("\n"));
    }
    for c in &p.what_changed {
        change_body(out, c);
    }
    if p.failed {
        let device = p.device_text.as_deref().map(str::trim_end);
        let _ = writeln!(
            out,
            "### unreadable\n{}",
            device.filter(|t| !t.is_empty()).unwrap_or("no device text")
        );
    }
}

fn change_body(out: &mut String, c: &BlockChange) {
    let id = block_id(&c.block);
    let tag = if id.is_empty() {
        String::new()
    } else {
        format!(" #{id}")
    };
    if let Some(unknown) = &c.before_unknown {
        let _ = writeln!(out, "### beforeUnknown{tag}\n{unknown}");
    }
    if let Some(diff) = &c.word_diff {
        let _ = writeln!(out, "### wordDiff{tag}\n{diff}");
    }
    if let Some(before) = &c.text_before {
        let _ = writeln!(out, "### textBefore{tag}\n{}", before.trim_end());
    }
    if let Some(now) = &c.text_full_now {
        let _ = writeln!(out, "### textNow{tag}\n{}", now.trim_end());
    }
}

/// The manifest as TOON.
pub fn manifest_text(manifest: &Manifest) -> String {
    toon::render(&serde_json::to_value(manifest).unwrap_or(Value::Null))
}

/// The note as an OKF document: frontmatter, then `## Page N` per page with the anchor of
/// every block as an HTML comment before its text.
///
/// `generated.model` is the model of the newest page version, empty when the state does not
/// know it. `source_sha256` is empty (the state keeps only the md5 of the file). Digests are
/// left out: the state holds only their ids.
pub fn note_document(note: &NoteState, slug: &str) -> String {
    let pages = note.pages.len();
    let description = if pages == 1 {
        "Handwritten Supernote note, 1 page, read by OCR.".to_string()
    } else {
        format!("Handwritten Supernote note, {pages} pages, read by OCR.")
    };
    let now = time::OffsetDateTime::now_utc();
    let at = now.replace_nanosecond(0).unwrap_or(now);
    let fm = Frontmatter {
        title: note.title.clone(),
        description,
        resource: note.path.clone(),
        generated_by: format!("notedown/{}", env!("CARGO_PKG_VERSION")),
        generated_model: note.model.clone().unwrap_or_default(),
        generated_at: at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        source_date: None,
        last_modified: None,
        status: if note
            .pages
            .iter()
            .any(|p| p.state == PageOcrState::Truncated)
        {
            "draft"
        } else {
            "stable"
        }
        .into(),
        source_sha256: String::new(),
        file_id: None,
        pages,
        keywords: note.meta.keywords.clone(),
        links: note
            .meta
            .links
            .iter()
            .map(|l| FrontmatterLink {
                page: l.page + 1,
                target_file_id: l.key.clone().unwrap_or_default(),
                target_path: l.target.clone(),
                target_md: None,
            })
            .collect(),
        // Left out until the note state holds the page, text and rectangle of each digest.
        digests: Vec::new(),
        extra: Vec::new(),
        format: HeaderFormat::Okf,
        crlf: false,
    };
    let outs: Vec<PageOut> = note
        .pages
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let markdown = p
                .blocks
                .iter()
                .map(|b| {
                    format!(
                        "<!-- notedown://{slug}/{}/{}#{} -->\n{}",
                        note.key,
                        p.page_id,
                        b.id,
                        b.text.trim_end()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            PageOut {
                number: i + 1,
                page_id: Some(p.page_id.clone()),
                image_rel: None,
                markdown,
                truncated: p.state == PageOcrState::Truncated,
                empty: p.blocks.is_empty(),
            }
        })
        .collect();
    render_document(&fm, &outs)
}

/// The status of the store as TOON.
pub fn status_text(status: &Value) -> String {
    toon::render(status)
}

/// The notes of `list_notes` as TOON.
pub fn notes_text(notes: &[Value]) -> String {
    toon::render(&json!({ "notes": notes }))
}

/// The hits of `search_notes` as TOON.
pub fn search_text(hits: &[Value]) -> String {
    toon::render(&json!({ "hits": hits }))
}

/// Pretty JSON of the same structs, for the CLI.
pub fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::{BlockChange, ManifestNote, NoteDelta, PageDelta};
    use crate::model::{Block, Change, MetaLink, NoteMeta, PageOcrState, PageState};

    const A: &str = "notedown://jordi/F2024/P2024";

    fn block(id: &str, text: &str) -> Block {
        Block {
            id: id.into(),
            label: "text".into(),
            bbox: [0, 0, 1000, 1000],
            text: text.into(),
            replaces: Vec::new(),
        }
    }

    fn change(id: &str, change: Change) -> BlockChange {
        BlockChange {
            block: format!("{A}#{id}"),
            change,
            word_diff: None,
            text_before: None,
            text_full_now: None,
            replaced_by: Vec::new(),
            before_unknown: None,
        }
    }

    fn planview() -> Delta {
        let edited = BlockChange {
            word_diff: Some("Meeting with [-Anna-][+Anne+] on Friday".into()),
            ..change("b3", Change::Edited)
        };
        let deleted = BlockChange {
            text_before: Some("call Bob".into()),
            ..change("b5", Change::Deleted)
        };
        let replaced = BlockChange {
            text_before: Some("Flow: A then B".into()),
            replaced_by: vec![format!("{A}#b7")],
            ..change("b4", Change::Replaced)
        };
        Delta {
            slug: "jordi".into(),
            from: 412,
            up_to: 455,
            more: false,
            range_text:
                "since your last confirm on 2026-10-08 21:14 +02:00 (seq 412) up to seq 455".into(),
            notes: vec![NoteDelta {
                key: "F2024".into(),
                old_note: None,
                path: "NOTE/Note/Planview.note".into(),
                old_path: None,
                title: "Planview".into(),
                keywords: vec!["plan".into()],
                links: Vec::new(),
                change: Change::Edited,
                pending: false,
                pages: vec![PageDelta {
                    page: 3,
                    page_id: "P2024".into(),
                    change: Change::Edited,
                    signal: Vec::new(),
                    text_now: vec![
                        block("b3", "Meeting with Anne on Friday"),
                        block("b7", "Flow: A then C\nsecond line"),
                    ],
                    what_changed: vec![edited, deleted, replaced],
                    new_blocks: 0,
                    failed: false,
                    device_text: None,
                }],
            }],
        }
    }

    #[test]
    fn the_delta_text_matches_the_spec_example() {
        let want = "\
range: \"since your last confirm on 2026-10-08 21:14 +02:00 (seq 412) up to seq 455: 1 page changed in 1 note\"
from: 412
upTo: 455
more: false
legend: \"textBefore is the text when you last confirmed (or at your since); wordDiff: [-...-] removed, [+...+] added\"
notes[1]{key,title,path,change,pending,oldNote,oldPath,keywords,links}:
  F2024,Planview,NOTE/Note/Planview.note,edited,false,null,null,plan,null
blocks[3]{note,page,block,change,replacedBy}:
  NOTE/Note/Planview,3,\"notedown://jordi/F2024/P2024#b3\",edited,null
  NOTE/Note/Planview,3,\"notedown://jordi/F2024/P2024#b5\",deleted,null
  NOTE/Note/Planview,3,\"notedown://jordi/F2024/P2024#b4\",replaced,\"notedown://jordi/F2024/P2024#b7\"

## NOTE/Note/Planview, page 3 (edited)
### textNow
<!-- notedown://jordi/F2024/P2024#b3 -->
Meeting with Anne on Friday

<!-- notedown://jordi/F2024/P2024#b7 -->
Flow: A then C
second line
### wordDiff #b3
Meeting with [-Anna-][+Anne+] on Friday
### textBefore #b5
call Bob
### textBefore #b4
Flow: A then B
";
        assert_eq!(delta_text(&planview()), want);
    }

    #[test]
    fn multi_line_text_never_lands_in_a_toon_cell() {
        let mut d = planview();
        let page = &mut d.notes[0].pages[0];
        page.what_changed[1].text_before = Some("call Bob\nand Eve".into());
        let text = delta_text(&d);
        let head = text.split("\n\n## ").next().unwrap();
        assert!(!head.contains("Bob"), "{head}");
        assert!(!head.contains("\\n"), "{head}");
        assert!(
            text.contains("### textBefore #b5\ncall Bob\nand Eve\n"),
            "{text}"
        );
    }

    #[test]
    fn a_full_replacement_shows_the_new_text_and_a_moved_page_has_no_text() {
        let mut d = planview();
        let full = BlockChange {
            text_before: Some("old words here".into()),
            text_full_now: Some("entirely new words".into()),
            ..change("b3", Change::Edited)
        };
        d.notes[0].pages[0].what_changed = vec![full];
        d.notes[0].pages.push(PageDelta {
            page: 5,
            page_id: "P9".into(),
            change: Change::Moved,
            signal: Vec::new(),
            text_now: Vec::new(),
            what_changed: Vec::new(),
            new_blocks: 0,
            failed: false,
            device_text: None,
        });
        let text = delta_text(&d);
        assert!(
            text.contains(
                "### textBefore #b3\nold words here\n### textNow #b3\nentirely new words\n"
            ),
            "{text}"
        );
        assert!(
            text.ends_with("## NOTE/Note/Planview, page 5 (moved)\n"),
            "{text}"
        );
        assert!(text.contains("2 pages changed in 1 note"), "{text}");
    }

    #[test]
    fn an_unreadable_page_shows_the_device_text() {
        let mut d = planview();
        let failed = |page: usize, id: &str, device: Option<&str>| PageDelta {
            page,
            page_id: id.into(),
            change: Change::Edited,
            signal: Vec::new(),
            text_now: Vec::new(),
            what_changed: Vec::new(),
            new_blocks: 0,
            failed: true,
            device_text: device.map(str::to_string),
        };
        d.notes[0]
            .pages
            .push(failed(4, "P4", Some("call Bob\nabout March\n")));
        d.notes[0].pages.push(failed(5, "P5", None));
        let text = delta_text(&d);
        assert!(
            text.contains(
                "## NOTE/Note/Planview, page 4 (edited, unreadable)\n### unreadable\ncall Bob\nabout March\n"
            ),
            "{text}"
        );
        assert!(
            text.ends_with(
                "## NOTE/Note/Planview, page 5 (edited, unreadable)\n### unreadable\nno device text\n"
            ),
            "{text}"
        );
        // Unreadable pages are no changed pages, and no block rows.
        assert!(
            text.contains("1 page changed in 1 note, 2 pages unreadable"),
            "{text}"
        );
        assert!(text.contains("blocks[3]"), "{text}");
        let head = text.split("\n\n## ").next().unwrap();
        assert!(!head.contains("about March"), "{head}");
    }

    #[test]
    fn two_notes_with_one_title_are_told_apart() {
        let mut d = planview();
        let mut other = d.notes[0].clone();
        other.key = "F2025".into();
        other.path = "NOTE/Work/Planview.note".into();
        d.notes.push(other);
        let text = delta_text(&d);
        assert!(text.contains("## NOTE/Note/Planview, page 3"), "{text}");
        assert!(text.contains("## NOTE/Work/Planview, page 3"), "{text}");
        assert!(
            text.contains("  NOTE/Work/Planview,3,\"notedown://jordi/F2024/P2024#b3\""),
            "{text}"
        );
        let mut top = planview();
        top.notes[0].path = "Planview.note".into();
        assert!(delta_text(&top).contains("## Planview, page 3"));
    }

    fn note() -> NoteState {
        NoteState {
            key: "F2024".into(),
            path: "NOTE/Note/Planview.note".into(),
            title: "Planview".into(),
            md5: "abc".into(),
            model: Some("m1".into()),
            meta: NoteMeta {
                keywords: vec!["plan".into()],
                links: vec![MetaLink {
                    page: 0,
                    target: "NOTE/Note/Other.note".into(),
                    key: Some("F9".into()),
                }],
                digests: vec!["D1".into()],
                ..NoteMeta::default()
            },
            pages: vec![
                PageState {
                    page_id: "P1".into(),
                    position: 0,
                    state: PageOcrState::Done,
                    blocks: vec![block("b1", "Hello"), block("b2", "World")],
                    ..PageState::default()
                },
                PageState {
                    page_id: "P2".into(),
                    position: 1,
                    state: PageOcrState::Pending,
                    ..PageState::default()
                },
            ],
            ..NoteState::default()
        }
    }

    #[test]
    fn the_note_document_is_okf_with_anchors() {
        let doc = note_document(&note(), "jordi");
        assert!(
            doc.starts_with("---\ntype: note\ntitle: \"Planview\"\n"),
            "{doc}"
        );
        assert!(
            doc.contains("resource: \"NOTE/Note/Planview.note\"\n"),
            "{doc}"
        );
        assert!(doc.contains("tags: [supernote]\n"), "{doc}");
        assert!(doc.contains("by: notedown/"), "{doc}");
        assert!(doc.contains("model: \"m1\""), "{doc}");
        assert!(doc.contains("  - \"plan\"\n"), "{doc}");
        assert!(
            doc.contains("target_path: \"NOTE/Note/Other.note\""),
            "{doc}"
        );
        assert!(!doc.contains("digests:"), "{doc}");
        assert!(doc.contains("pages: 2\n"), "{doc}");
        assert!(
            doc.contains(
                "## Page 1\n<!-- page_id: P1 -->\n\n<!-- notedown://jordi/F2024/P1#b1 -->\nHello\n\n<!-- notedown://jordi/F2024/P1#b2 -->\nWorld\n"
            ),
            "{doc}"
        );
        assert!(doc.contains("## Page 2\n"), "{doc}");
    }

    #[test]
    fn cli_json_and_toon_hold_the_same_values() {
        let m = Manifest {
            from: 1,
            up_to: 9,
            more: true,
            notes: vec![ManifestNote {
                key: "F1".into(),
                path: "NOTE/a.note".into(),
                folder: "NOTE".into(),
                change: Change::New,
                pages: 2,
                blocks: 5,
                pending: false,
                failed: false,
                chars: 120,
            }],
        };
        let json: Value = serde_json::from_str(&to_json(&m)).unwrap();
        let toon = manifest_text(&m);
        assert_eq!(json["upTo"], 9);
        assert!(toon.contains("upTo: 9"), "{toon}");
        assert!(toon.contains("more: true"), "{toon}");
        let n = &json["notes"][0];
        let row = format!(
            "{},{},{},{},{},{},{},{},{}",
            n["blocks"],
            n["change"].as_str().unwrap(),
            n["chars"],
            n["failed"],
            n["folder"].as_str().unwrap(),
            n["key"].as_str().unwrap(),
            n["pages"],
            n["path"].as_str().unwrap(),
            n["pending"]
        );
        assert!(toon.contains(&row), "{toon}\n{row}");
        let d = planview();
        let dj: Value = serde_json::from_str(&to_json(&d)).unwrap();
        assert_eq!(
            dj["notes"][0]["pages"][0]["whatChanged"][0]["block"],
            format!("{A}#b3")
        );
    }

    #[test]
    fn the_note_document_round_trips_through_the_frontmatter_parser() {
        let n = note();
        let fm = Frontmatter::parse(&note_document(&n, "jordi")).expect("parses");
        assert_eq!(fm.title, n.title);
        assert_eq!(fm.resource, n.path);
        assert_eq!(fm.generated_model, "m1");
        assert_eq!(fm.pages, n.pages.len());
        assert_eq!(fm.keywords, n.meta.keywords);
        assert_eq!(fm.links.len(), 1);
        assert_eq!(fm.links[0].page, 1);
        assert_eq!(fm.links[0].target_path, "NOTE/Note/Other.note");
        assert!(fm.digests.is_empty());
        let mut unknown = n;
        unknown.model = None;
        let fm = Frontmatter::parse(&note_document(&unknown, "jordi")).expect("parses");
        assert_eq!(fm.generated_model, "");
    }
}
