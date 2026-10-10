use std::collections::BTreeMap;

use super::*;
use crate::model::{LineChange, LogLine, NoteState, PageOcrState, PageState};
use crate::store::WriterLock;
use crate::text::normalize;

const A: &str = "F20240424175151947577oMie9HNQYnTa";
const B: &str = "F20240424175151947577oMie9HNQYnTb";
const C: &str = "F20240424175151947577oMie9HNQYnTc";
/// 2026-07-01 08:30 UTC.
const JULY: u64 = 1_782_894_600_000;
const DAY: u64 = 24 * 60 * 60 * 1000;

struct Fx {
    _tmp: tempfile::TempDir,
    w: WriterLock,
    store: TextStore,
}

fn fx() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let w = WriterLock::take(tmp.path()).unwrap();
    let store = TextStore::open_writer(tmp.path(), "jordi", &w).unwrap();
    Fx {
        _tmp: tmp,
        w,
        store,
    }
}

fn b(id: &str, text: &str) -> Block {
    Block {
        id: id.into(),
        label: "text".into(),
        bbox: [0, 0, 10, 10],
        text: text.into(),
        replaces: Vec::new(),
    }
}

fn replacing(id: &str, text: &str, old: &[&str]) -> Block {
    let mut block = b(id, text);
    block.replaces = old.iter().map(|s| s.to_string()).collect();
    block
}

fn name(note: &str) -> &'static str {
    match note {
        A => "Alpha",
        B => "Beta",
        _ => "Gamma",
    }
}

fn page(note: &str, page: &str, change: LineChange, blocks: Vec<Block>) -> LogLine {
    let mut line = LogLine::new(note, change);
    line.page_id = Some(page.into());
    line.position = Some(page[1..].parse::<usize>().unwrap() - 1);
    line.path = Some(format!("Notes/{}.note", name(note)));
    line.blocks = blocks;
    line
}

fn at(mut line: LogLine, ts: u64) -> LogLine {
    line.ts = ts;
    line
}

fn anchor(note: &str, page: &str, block: &str) -> String {
    format!("notedown://jordi/{note}/{page}#{block}")
}

impl Fx {
    fn add(&self, lines: Vec<LogLine>) {
        let mut lines = lines;
        self.store.append(&self.w, &mut lines).unwrap();
    }

    fn state(&self, key: &str, pages: Vec<(&str, PageOcrState, Option<u64>)>) {
        let note = NoteState {
            key: key.into(),
            path: format!("Notes/{}.note", name(key)),
            title: name(key).into(),
            pages: pages
                .into_iter()
                .enumerate()
                .map(|(i, (id, state, seq))| PageState {
                    page_id: id.into(),
                    position: i,
                    state,
                    seq,
                    ..PageState::default()
                })
                .collect(),
            ..NoteState::default()
        };
        self.store.save_note(&self.w, &note).unwrap();
    }

    fn delta(&self, start: &Start) -> Delta {
        delta(&self.store, start, None, MAX_CHARS, UtcOffset::UTC).unwrap()
    }
}

fn bookmark(name: &str) -> Start {
    Start::Bookmark(name.into())
}

fn note<'a>(d: &'a Delta, key: &str) -> &'a NoteDelta {
    d.notes
        .iter()
        .find(|n| n.key == key)
        .unwrap_or_else(|| panic!("{key} not in {d:#?}"))
}

#[test]
fn several_versions_merge_into_one_change() {
    let f = fx();
    let one = || b("b2", "call Bob");
    f.add(vec![page(
        A,
        "P1",
        LineChange::New,
        vec![b("b1", "Meeting with Anna on Friday"), one()],
    )]);
    f.store.ack("c", 1).unwrap();
    f.add(vec![
        page(
            A,
            "P1",
            LineChange::Edited,
            vec![b("b1", "Meeting with Anna on Monday"), one()],
        ),
        page(
            A,
            "P1",
            LineChange::Edited,
            vec![b("b1", "Meeting with Anne on Monday"), one()],
        ),
    ]);
    let d = f.delta(&bookmark("c"));
    assert_eq!((d.from, d.up_to, d.more), (1, 3, false));
    assert!(
        d.range_text.starts_with("since your last confirm on ")
            && d.range_text.ends_with("(seq 1) up to seq 3"),
        "{}",
        d.range_text
    );
    assert_eq!(d.notes.len(), 1);
    let n = &d.notes[0];
    assert_eq!((n.change, n.title.as_str()), (Change::Edited, "Alpha"));
    assert_eq!(n.pages.len(), 1);
    let p = &n.pages[0];
    assert_eq!((p.page, p.change), (1, Change::Edited));
    assert_eq!(p.text_now.len(), 2);
    assert_eq!(p.text_now[0].text, "Meeting with Anne on Monday");
    assert_eq!(
        p.what_changed,
        vec![BlockChange {
            block: anchor(A, "P1", "b1"),
            change: Change::Edited,
            word_diff: Some("Meeting with [-Anna-][+Anne+] on [-Friday-][+Monday+]".into()),
            text_before: None,
            text_full_now: None,
            replaced_by: vec![],
            before_unknown: None,
        }]
    );
    let json = serde_json::to_string(&d).unwrap();
    for name in ["\"textNow\"", "\"whatChanged\"", "\"wordDiff\"", "\"upTo\""] {
        assert!(json.contains(name), "{name} in {json}");
    }
}

#[test]
fn a_block_written_and_erased_in_between_does_not_appear() {
    let f = fx();
    f.add(vec![page(
        A,
        "P1",
        LineChange::New,
        vec![b("b1", "one two three")],
    )]);
    f.store.ack("c", 1).unwrap();
    f.add(vec![
        page(
            A,
            "P1",
            LineChange::Edited,
            vec![b("b1", "one two three"), b("b2", "a passing thought")],
        ),
        page(A, "P1", LineChange::Edited, vec![b("b1", "one two four")]),
    ]);
    let d = f.delta(&bookmark("c"));
    let p = &note(&d, A).pages[0];
    assert_eq!(p.text_now, vec![b("b1", "one two four")]);
    assert_eq!(p.what_changed.len(), 1, "{:#?}", p.what_changed);
    assert_eq!(p.what_changed[0].block, anchor(A, "P1", "b1"));
    assert!(!serde_json::to_string(&d).unwrap().contains("passing"));
}

#[test]
fn a_lineage_chain_is_merged() {
    let f = fx();
    f.add(vec![page(
        A,
        "P1",
        LineChange::New,
        vec![b("b1", "head"), b("b3", "alpha beta gamma")],
    )]);
    f.store.ack("c", 1).unwrap();
    f.add(vec![
        page(
            A,
            "P1",
            LineChange::Edited,
            vec![
                b("b1", "head"),
                replacing("b7", "alpha beta delta", &["b3"]),
            ],
        ),
        page(
            A,
            "P1",
            LineChange::Edited,
            vec![
                b("b1", "head"),
                replacing("b9", "something else entirely now", &["b7"]),
            ],
        ),
    ]);
    let d = f.delta(&bookmark("c"));
    let p = &note(&d, A).pages[0];
    assert_eq!(
        p.what_changed,
        vec![BlockChange {
            block: anchor(A, "P1", "b3"),
            change: Change::Replaced,
            word_diff: None,
            text_before: Some("alpha beta gamma".into()),
            text_full_now: None,
            replaced_by: vec![anchor(A, "P1", "b9")],
            before_unknown: None,
        }]
    );
    assert!(
        serde_json::to_string(&d)
            .unwrap()
            .contains("\"replacedBy\"")
    );
}

#[test]
fn a_deleted_note_lists_its_blocks() {
    let f = fx();
    f.add(vec![page(
        A,
        "P1",
        LineChange::New,
        vec![b("b1", "first"), b("b2", "second")],
    )]);
    f.store.ack("c", 1).unwrap();
    f.add(vec![LogLine::new(A, LineChange::NoteDeleted)]);
    let d = f.delta(&bookmark("c"));
    let n = note(&d, A);
    assert_eq!(n.change, Change::Deleted);
    assert_eq!(n.path, "Notes/Alpha.note");
    let p = &n.pages[0];
    assert_eq!(p.change, Change::Deleted);
    assert!(p.text_now.is_empty());
    let gone: Vec<(String, Change, Option<String>)> = p
        .what_changed
        .iter()
        .map(|c| (c.block.clone(), c.change, c.text_before.clone()))
        .collect();
    assert_eq!(
        gone,
        vec![
            (anchor(A, "P1", "b1"), Change::Deleted, Some("first".into())),
            (
                anchor(A, "P1", "b2"),
                Change::Deleted,
                Some("second".into())
            ),
        ]
    );
}

#[test]
fn created_and_deleted_inside_the_range_is_left_out() {
    let f = fx();
    f.add(vec![page(A, "P1", LineChange::New, vec![b("b1", "one")])]);
    f.store.ack("c", 1).unwrap();
    f.add(vec![
        page(B, "P1", LineChange::New, vec![b("b1", "short lived")]),
        LogLine::new(B, LineChange::NoteDeleted),
        page(A, "P1", LineChange::Edited, vec![b("b1", "two")]),
    ]);
    let d = f.delta(&bookmark("c"));
    let keys: Vec<&str> = d.notes.iter().map(|n| n.key.as_str()).collect();
    assert_eq!(keys, vec![A]);
    assert_eq!(d.up_to, 4);
}

#[test]
fn since_before_the_oldest_version_gives_before_unknown() {
    let f = fx();
    // P1's first kept line is an edit: its older versions were pruned.
    f.add(vec![
        at(
            page(A, "P1", LineChange::Edited, vec![b("b1", "kept")]),
            JULY,
        ),
        at(
            page(A, "P2", LineChange::New, vec![b("b2", "fresh")]),
            JULY + DAY,
        ),
    ]);
    let d = f.delta(&Start::Since(Since::Time(JULY - 10 * DAY)));
    assert_eq!((d.from, d.up_to), (0, 2));
    let n = note(&d, A);
    let p1 = n.pages.iter().find(|p| p.page_id == "P1").unwrap();
    assert_eq!(p1.text_now, vec![b("b1", "kept")]);
    assert_eq!(p1.what_changed.len(), 1);
    assert_eq!(
        p1.what_changed[0].before_unknown.as_deref(),
        Some("no versions before 2026-07-01 08:30 +00:00")
    );
    let p2 = n.pages.iter().find(|p| p.page_id == "P2").unwrap();
    assert_eq!(p2.change, Change::New);
    assert!(p2.what_changed.is_empty());
    assert!(
        serde_json::to_string(&d)
            .unwrap()
            .contains("\"beforeUnknown\"")
    );

    // A since inside the range of P1's kept lines knows its base.
    let d = f.delta(&Start::Since(Since::Time(JULY + 1)));
    assert_eq!(d.from, 1);
    assert!(note(&d, A).pages.iter().all(|p| p.page_id == "P2"));

    // An unknown consumer is the first import: everything is new.
    let d = f.delta(&bookmark("fresh"));
    let n = note(&d, A);
    assert_eq!(n.change, Change::New);
    assert!(
        n.pages
            .iter()
            .all(|p| p.change == Change::New && p.what_changed.is_empty())
    );
    assert!(d.range_text.ends_with("up to seq 2"), "{}", d.range_text);
}

#[test]
fn a_pending_page_is_listed_and_does_not_hold_up_to() {
    let f = fx();
    f.add(vec![
        page(A, "P1", LineChange::New, vec![b("b1", "read")]),
        page(B, "P1", LineChange::New, vec![b("b1", "other")]),
    ]);
    f.state(
        A,
        vec![
            ("P1", PageOcrState::Done, Some(1)),
            ("P2", PageOcrState::Pending, None),
        ],
    );
    f.state(B, vec![("P1", PageOcrState::Done, Some(2))]);
    // A note with no text yet: only pending pages.
    f.state(C, vec![("P1", PageOcrState::Pending, None)]);
    let d = f.delta(&bookmark("c"));
    assert_eq!((d.up_to, d.more), (2, false));
    let a = note(&d, A);
    assert!(a.pending);
    assert_eq!(a.pages.len(), 1);
    assert_eq!(a.pages[0].page_id, "P1");
    assert!(!note(&d, B).pending);
    let c = note(&d, C);
    assert!(c.pending && c.pages.is_empty());
    assert_eq!(c.change, Change::New);
}

#[test]
fn a_fixed_range_ignores_newer_lines() {
    let f = fx();
    f.add(vec![
        page(
            A,
            "P1",
            LineChange::New,
            vec![b("b1", "one two three four")],
        ),
        page(
            A,
            "P1",
            LineChange::Edited,
            vec![b("b1", "one two three five")],
        ),
        page(A, "P1", LineChange::Edited, vec![b("b1", "six")]),
    ]);
    let d = f.delta(&Start::Range { from: 1, up_to: 2 });
    assert_eq!((d.from, d.up_to, d.more), (1, 2, true));
    assert_eq!(d.range_text, "seq 1 to seq 2");
    let p = &note(&d, A).pages[0];
    assert_eq!(p.text_now, vec![b("b1", "one two three five")]);
    assert_eq!(
        p.what_changed[0].word_diff.as_deref(),
        Some("one two three [-four-][+five+]")
    );
    // A tiny max_chars does not cut a fixed range.
    let small = delta(
        &f.store,
        &Start::Range { from: 0, up_to: 3 },
        None,
        1,
        UtcOffset::UTC,
    )
    .unwrap();
    assert_eq!((small.up_to, small.more), (3, false));
    assert!(
        delta(
            &f.store,
            &Start::Range { from: 3, up_to: 2 },
            None,
            MAX_CHARS,
            UtcOffset::UTC
        )
        .is_err()
    );
}

#[test]
fn notes_filter() {
    let f = fx();
    f.add(vec![
        page(A, "P1", LineChange::New, vec![b("b1", "a")]),
        page(B, "P1", LineChange::New, vec![b("b1", "b")]),
        page(C, "P1", LineChange::New, vec![b("b1", "c")]),
    ]);
    let only = [B.to_string()];
    let d = delta(
        &f.store,
        &Start::Range { from: 0, up_to: 3 },
        Some(&only),
        MAX_CHARS,
        UtcOffset::UTC,
    )
    .unwrap();
    let keys: Vec<&str> = d.notes.iter().map(|n| n.key.as_str()).collect();
    assert_eq!(keys, vec![B]);
    assert_eq!(d.up_to, 3);
    // The cut does not depend on the filter.
    let cut = delta(&f.store, &bookmark("c"), Some(&only), 1, UtcOffset::UTC).unwrap();
    assert_eq!((cut.up_to, cut.more), (1, true));
    assert!(cut.notes.is_empty());
}

#[test]
fn the_manifest_holds_no_text() {
    let f = fx();
    f.add(vec![
        page(A, "P1", LineChange::New, vec![b("b1", "secret alpha")]),
        page(B, "P1", LineChange::New, vec![b("b1", "secret beta")]),
    ]);
    f.store.ack("c", 2).unwrap();
    f.add(vec![
        page(
            A,
            "P1",
            LineChange::Edited,
            vec![b("b1", "secret alpha two"), b("b2", "secret new")],
        ),
        page(A, "P2", LineChange::New, vec![b("b3", "secret page")]),
        LogLine::new(B, LineChange::NoteDeleted),
    ]);
    let m = manifest(&f.store, &bookmark("c"), UtcOffset::UTC).unwrap();
    assert_eq!((m.from, m.up_to, m.more), (2, 5, false));
    let json = serde_json::to_string(&m).unwrap();
    assert!(!json.contains("secret"), "{json}");
    assert!(!json.contains("textNow"), "{json}");
    let a = m.notes.iter().find(|n| n.key == A).unwrap();
    assert_eq!(
        (a.path.as_str(), a.folder.as_str(), a.change),
        ("Notes/Alpha.note", "Notes", Change::Edited)
    );
    // P1: b1 edited, b2 new; P2: b3 new.
    assert_eq!((a.pages, a.blocks, a.pending), (2, 3, false));
    assert!(a.chars > 0);
    let bn = m.notes.iter().find(|n| n.key == B).unwrap();
    assert_eq!((bn.change, bn.pages, bn.blocks), (Change::Deleted, 1, 1));
    // Every sub-agent sees the manifest's state.
    f.add(vec![page(
        A,
        "P1",
        LineChange::Edited,
        vec![b("b1", "later")],
    )]);
    for n in &m.notes {
        let keys = [n.key.clone()];
        let range = Start::Range {
            from: m.from,
            up_to: m.up_to,
        };
        let d = delta(&f.store, &range, Some(&keys), MAX_CHARS, UtcOffset::UTC).unwrap();
        assert_eq!(d.notes.len(), 1);
        let nd = &d.notes[0];
        let blocks: usize = nd
            .pages
            .iter()
            .map(|p| p.what_changed.len() + p.new_blocks)
            .sum();
        assert_eq!(
            (nd.change, nd.pages.len(), blocks),
            (n.change, n.pages, n.blocks)
        );
    }
}

/// The old text of a word diff: `[+...+]` dropped, `[-...-]` kept.
fn old_of_diff(diff: &str) -> String {
    let mut out = String::new();
    let mut rest = diff;
    while let Some(i) = rest.find("[+") {
        out.push_str(&rest[..i]);
        let end = rest[i..].find("+]").unwrap();
        rest = &rest[i + end + 2..];
    }
    out.push_str(rest);
    normalize(&out.replace("[-", " ").replace("-]", " "))
}

#[test]
fn looping_with_ack_shows_every_change_once() {
    let f = fx();
    // Every line holds two blocks of 18 characters, so a cut of 4 lines is 144 characters.
    let t = |n: &str, v: u32, which: &str| format!("{n} {which} version {v:02} x");
    let mut lines = Vec::new();
    for v in 1..=4u32 {
        for n in [A, B, C] {
            let short = &name(n)[..1];
            let change = if v == 1 {
                LineChange::New
            } else {
                LineChange::Edited
            };
            let blocks = match (n, v) {
                // B's second block is replaced by b5 in version 3.
                (B, 3..) => vec![
                    b("b1", &t(short, v, "one")),
                    replacing("b5", &t(short, v, "fiv"), &["b2"]),
                ],
                // C's second block is erased in version 3 and a new one comes in 4.
                (C, 3) => vec![b("b1", &t(short, v, "one")), b("b3", &t(short, v, "thr"))],
                (C, 4) => vec![b("b1", &t(short, 3, "one")), b("b4", &t(short, v, "fou"))],
                // A changes only its second block in version 4.
                (A, 4) => vec![b("b1", &t(short, 3, "one")), b("b2", &t(short, v, "two"))],
                _ => vec![b("b1", &t(short, v, "one")), b("b2", &t(short, v, "two"))],
            };
            for block in &blocks {
                assert_eq!(block.text.chars().count(), 18, "{}", block.text);
            }
            lines.push(page(n, "P1", change, blocks));
        }
    }
    let last_text: BTreeMap<String, Vec<Block>> = [A, B, C]
        .iter()
        .map(|n| {
            let l = lines.iter().rev().find(|l| l.note == *n).unwrap();
            (n.to_string(), l.blocks.clone())
        })
        .collect();
    f.add(lines);

    // The agent's knowledge: per note the text of its blocks.
    let mut model: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut answers = Vec::new();
    let mut previous = 0;
    loop {
        let d = delta(&f.store, &bookmark("agent"), None, 144, UtcOffset::UTC).unwrap();
        assert_eq!(d.from, previous, "answers are contiguous");
        assert!(d.up_to > d.from);
        for n in &d.notes {
            let known = model.entry(n.key.clone()).or_default();
            for p in &n.pages {
                let id_of = |a: &str| a.rsplit('#').next().unwrap().to_string();
                match p.change {
                    Change::New => assert!(known.is_empty(), "new twice: {n:#?}"),
                    Change::Edited => {
                        assert!(!known.is_empty(), "an edit of an unknown page");
                        for c in &p.what_changed {
                            let id = id_of(&c.block);
                            let before = known.get(&id).unwrap_or_else(|| {
                                panic!("{id} unknown to the agent in {:#?}", d.notes)
                            });
                            let expected = match (c.change, &c.word_diff) {
                                (Change::Edited, Some(diff)) => old_of_diff(diff),
                                _ => normalize(c.text_before.as_deref().unwrap()),
                            };
                            assert_eq!(normalize(before), expected, "{c:#?}");
                        }
                        // Every block the agent knows that changed has an entry.
                        let now: BTreeMap<&str, &str> = p
                            .text_now
                            .iter()
                            .map(|bl| (bl.id.as_str(), bl.text.as_str()))
                            .collect();
                        for (id, text) in known.iter() {
                            if now.get(id.as_str()) != Some(&text.as_str()) {
                                assert!(
                                    p.what_changed.iter().any(|c| id_of(&c.block) == *id),
                                    "{id} changed without an entry"
                                );
                            }
                        }
                    }
                    other => panic!("unexpected {other:?}"),
                }
                *known = p
                    .text_now
                    .iter()
                    .map(|bl| (bl.id.clone(), bl.text.clone()))
                    .collect();
            }
        }
        f.store.ack("agent", d.up_to).unwrap();
        previous = d.up_to;
        answers.push(d.clone());
        if !d.more {
            break;
        }
        assert!(answers.len() < 10, "the loop ends");
    }
    assert_eq!(answers.len(), 3, "12 lines, 4 per answer");
    assert_eq!(answers.last().unwrap().up_to, 12);
    // Cuts split every note: each note shows in every answer.
    for d in &answers {
        assert_eq!(d.notes.len(), 3);
    }
    let end: BTreeMap<String, BTreeMap<String, String>> = last_text
        .into_iter()
        .map(|(k, blocks)| (k, blocks.into_iter().map(|bl| (bl.id, bl.text)).collect()))
        .collect();
    assert_eq!(model, end);
    // Nothing more after the last ack.
    let d = f.delta(&bookmark("agent"));
    assert!(d.notes.is_empty() && !d.more && d.up_to == 12);
}

#[test]
fn since_parses_times_dates_and_seqs() {
    let now = OffsetDateTime::from_unix_timestamp(1_791_000_000).unwrap();
    let two = UtcOffset::from_hms(2, 0, 0).unwrap();
    assert_eq!(parse_since("412", now, two).unwrap(), Since::Seq(412));
    assert_eq!(
        parse_since("2026-07-01T08:30:00Z", now, two).unwrap(),
        Since::Time(JULY)
    );
    // A date is local midnight.
    assert_eq!(
        parse_since("2026-07-01", now, two).unwrap(),
        Since::Time(JULY - (8 * 60 + 30) * 60_000 - 2 * 3_600_000)
    );
    assert!(parse_since("yesterday", now, two).is_err());
    assert!(parse_since("", now, two).is_err());
}

fn ack_to(f: &Fx, consumer: &str, d: &Delta) {
    f.store.ack(consumer, d.up_to).unwrap();
}

#[test]
fn a_page_deletion_of_a_textless_note_comes_once() {
    let f = fx();
    f.add(vec![page(
        A,
        "P1",
        LineChange::New,
        vec![b("b1", "first"), b("b2", "second")],
    )]);
    f.state(
        A,
        vec![
            ("P1", PageOcrState::Done, Some(1)),
            ("P2", PageOcrState::Pending, None),
        ],
    );
    f.store.ack("c", 1).unwrap();
    // P1 is removed while P2 still waits for OCR: no text is left, the note lives.
    f.add(vec![page(A, "P1", LineChange::Deleted, vec![])]);
    f.state(A, vec![("P2", PageOcrState::Pending, None)]);
    let d = f.delta(&bookmark("c"));
    let n = note(&d, A);
    assert_eq!((n.change, n.pending), (Change::Edited, true));
    assert_eq!(n.pages.len(), 1);
    let p = &n.pages[0];
    assert_eq!((p.page_id.as_str(), p.change), ("P1", Change::Deleted));
    let gone: Vec<(String, Change, Option<String>)> = p
        .what_changed
        .iter()
        .map(|c| (c.block.clone(), c.change, c.text_before.clone()))
        .collect();
    assert_eq!(
        gone,
        vec![
            (anchor(A, "P1", "b1"), Change::Deleted, Some("first".into())),
            (
                anchor(A, "P1", "b2"),
                Change::Deleted,
                Some("second".into())
            ),
        ]
    );
    ack_to(&f, "c", &d);
    // P2 gets its text: only P2 comes, as new.
    f.add(vec![page(A, "P2", LineChange::New, vec![b("b3", "third")])]);
    f.state(A, vec![("P2", PageOcrState::Done, Some(3))]);
    let d = f.delta(&bookmark("c"));
    let n = note(&d, A);
    assert!(!n.pending);
    assert_eq!(n.pages.len(), 1);
    assert_eq!(
        (n.pages[0].page_id.as_str(), n.pages[0].change),
        ("P2", Change::New)
    );
    assert_eq!(n.pages[0].text_now, vec![b("b3", "third")]);
}

#[test]
fn a_pending_note_whose_lines_cancel_out_is_still_listed() {
    let f = fx();
    f.add(vec![page(B, "P1", LineChange::New, vec![b("b1", "same")])]);
    f.store.ack("c", 1).unwrap();
    f.add(vec![
        page(B, "P1", LineChange::Edited, vec![b("b1", "other")]),
        page(B, "P1", LineChange::Edited, vec![b("b1", "same")]),
    ]);
    f.state(
        B,
        vec![
            ("P1", PageOcrState::Done, Some(3)),
            ("P2", PageOcrState::Pending, None),
        ],
    );
    let d = f.delta(&bookmark("c"));
    let n = note(&d, B);
    assert!(n.pending && n.pages.is_empty());
    assert_eq!(n.change, Change::Edited);
}

const OLD: &str = "path:0123456789abcdef0123456789abcdef";
const NEW: &str = "path:fedcba9876543210fedcba9876543210";

fn moved_to(note: &str, old_note: Option<&str>, path: &str, old_path: &str) -> LogLine {
    let mut line = LogLine::new(note, LineChange::NoteMoved);
    line.old_note = old_note.map(str::to_string);
    line.path = Some(path.into());
    line.old_path = Some(old_path.into());
    line
}

#[test]
fn a_re_keyed_note_keeps_its_lines() {
    let f = fx();
    f.add(vec![page(OLD, "P1", LineChange::New, vec![b("b1", "one")])]);
    f.store.ack("c", 1).unwrap();
    f.store.ack("cut", 1).unwrap();
    f.add(vec![
        page(OLD, "P1", LineChange::Edited, vec![b("b1", "one two")]),
        page(B, "P1", LineChange::New, vec![b("b1", "b")]),
        moved_to(NEW, Some(OLD), "Archive/Gamma.note", "Notes/Gamma.note"),
        // Page lines carry the path of the note now.
        LogLine {
            path: Some("Archive/Gamma.note".into()),
            ..page(
                NEW,
                "P1",
                LineChange::Edited,
                vec![b("b1", "one two three")],
            )
        },
        // A new note at the old path reuses the old key: another note.
        page(OLD, "P1", LineChange::New, vec![b("b1", "another note")]),
    ]);

    let d = f.delta(&bookmark("c"));
    let moved = note(&d, NEW);
    assert_eq!(moved.old_note.as_deref(), Some(OLD));
    assert_eq!(moved.change, Change::Edited);
    assert_eq!(
        (moved.path.as_str(), moved.old_path.as_deref()),
        ("Archive/Gamma.note", Some("Notes/Gamma.note"))
    );
    assert_eq!(moved.title, "Gamma");
    let p = &moved.pages[0];
    assert_eq!(p.text_now, vec![b("b1", "one two three")]);
    assert_eq!(p.what_changed[0].block, anchor(NEW, "P1", "b1"));
    // Two of three words are new: the texts in full, the base is the old key's text.
    assert_eq!(
        (
            p.what_changed[0].text_before.as_deref(),
            p.what_changed[0].text_full_now.as_deref()
        ),
        (Some("one"), Some("one two three"))
    );
    let reused = note(&d, OLD);
    assert_eq!(
        (reused.change, reused.old_note.as_deref()),
        (Change::New, None)
    );
    assert_eq!(reused.pages[0].text_now, vec![b("b1", "another note")]);
    assert!(serde_json::to_string(&d).unwrap().contains("\"oldNote\""));

    // A cut before the move: the old key's lines show under the old key, then the rest under
    // the new key, against the text of the first answer.
    let first = delta(&f.store, &bookmark("cut"), None, 7, UtcOffset::UTC).unwrap();
    assert_eq!((first.up_to, first.more), (2, true));
    let old = note(&first, OLD);
    assert_eq!(
        (old.change, old.old_note.as_deref()),
        (Change::Edited, None)
    );
    assert_eq!(old.pages[0].text_now, vec![b("b1", "one two")]);
    ack_to(&f, "cut", &first);
    let second = f.delta(&bookmark("cut"));
    let moved = note(&second, NEW);
    assert_eq!(moved.old_note.as_deref(), Some(OLD));
    assert_eq!(
        moved.pages[0].what_changed[0].word_diff.as_deref(),
        Some("one two [+three+]")
    );
    assert_eq!(note(&second, OLD).change, Change::New);
}

#[test]
fn unscoped_is_its_own_change() {
    let f = fx();
    f.add(vec![page(
        A,
        "P1",
        LineChange::New,
        vec![b("b1", "kept text")],
    )]);
    f.store.ack("c", 1).unwrap();
    f.add(vec![LogLine::new(A, LineChange::NoteUnscoped)]);
    let d = f.delta(&bookmark("c"));
    let n = note(&d, A);
    assert_eq!(n.change, Change::Unscoped);
    assert!(n.pages.is_empty(), "no block reads as deleted: {n:#?}");
    ack_to(&f, "c", &d);

    // Back in scope: the worker writes `new` lines with the same text.
    f.add(vec![
        page(A, "P1", LineChange::New, vec![b("b1", "kept text")]),
        // Created and unscoped inside the range: left out.
        page(B, "P1", LineChange::New, vec![b("b1", "brief")]),
        LogLine::new(B, LineChange::NoteUnscoped),
    ]);
    let d = f.delta(&bookmark("c"));
    let keys: Vec<&str> = d.notes.iter().map(|n| n.key.as_str()).collect();
    assert_eq!(keys, vec![A]);
    assert_eq!(note(&d, A).change, Change::New);
    assert!(note(&d, A).pages.is_empty());
}

#[test]
fn a_moved_page_and_a_moved_note() {
    let f = fx();
    f.add(vec![
        page(A, "P1", LineChange::New, vec![b("b1", "one")]),
        page(A, "P2", LineChange::New, vec![b("b2", "two")]),
        page(B, "P1", LineChange::New, vec![b("b1", "beta")]),
    ]);
    f.store.ack("c", 3).unwrap();
    let mut p1 = page(A, "P1", LineChange::Moved, vec![]);
    p1.position = Some(1);
    let mut p2 = page(A, "P2", LineChange::Moved, vec![]);
    p2.position = Some(0);
    f.add(vec![
        p1,
        p2,
        moved_to(B, None, "Archive/Beta.note", "Notes/Beta.note"),
    ]);
    let d = f.delta(&bookmark("c"));
    let a = note(&d, A);
    assert_eq!(a.change, Change::Edited);
    let pages: Vec<(usize, &str, Change, bool)> = a
        .pages
        .iter()
        .map(|p| {
            (
                p.page,
                p.page_id.as_str(),
                p.change,
                p.text_now.is_empty() && p.what_changed.is_empty(),
            )
        })
        .collect();
    assert_eq!(
        pages,
        vec![
            (1, "P2", Change::Moved, true),
            (2, "P1", Change::Moved, true)
        ]
    );
    let bn = note(&d, B);
    assert_eq!(bn.change, Change::Moved);
    assert_eq!(
        (bn.path.as_str(), bn.old_path.as_deref(), bn.title.as_str()),
        ("Archive/Beta.note", Some("Notes/Beta.note"), "Beta")
    );
    assert!(bn.pages.is_empty());
}

#[test]
fn dates_show_the_local_offset() {
    let f = fx();
    f.add(vec![at(
        page(A, "P1", LineChange::Edited, vec![b("b1", "kept")]),
        JULY,
    )]);
    let two = UtcOffset::from_hms(2, 0, 0).unwrap();
    let d = delta(
        &f.store,
        &Start::Since(Since::Time(JULY)),
        None,
        MAX_CHARS,
        two,
    )
    .unwrap();
    assert_eq!(
        d.range_text,
        "since 2026-07-01 10:30 +02:00 (seq 0) up to seq 1"
    );
    assert_eq!(
        note(&d, A).pages[0].what_changed[0]
            .before_unknown
            .as_deref(),
        Some("no versions before 2026-07-01 10:30 +02:00")
    );
}
