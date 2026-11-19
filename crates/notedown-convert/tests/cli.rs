use notedown_convert::links::FileIndex;
use notedown_convert::{
    document::{Frontmatter, FrontmatterDigest, FrontmatterLink, HeaderFormat},
    inputs, pipeline,
};
use notedown_ocr::{OcrEngine, OcrError, PageOcr};
use std::path::PathBuf;

struct FakeEngine {
    calls: usize,
}
impl OcrEngine for FakeEngine {
    fn ocr_png(&mut self, _png: &[u8]) -> Result<PageOcr, OcrError> {
        self.calls += 1;
        Ok(PageOcr {
            prompt_tokens: 10,
            output_tokens: 5,
            encode_ms: 1,
            decode_ms: 1,
            ..PageOcr::from_blocks(vec![
                notedown_ocr::OcrBlock {
                    label: "Text".into(),
                    bbox: None,
                    markdown: "Hello page".into(),
                },
                notedown_ocr::OcrBlock {
                    label: "Diagram".into(),
                    bbox: None,
                    markdown: "```mermaid\ngraph LR\nA --> B\n```".into(),
                },
            ])
        })
    }
    fn label(&self) -> &str {
        "fake-model"
    }
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

fn setup() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("in");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::copy(fixtures().join("notes/test2.note"), src.join("Test 2.note")).unwrap();
    std::fs::copy(
        fixtures().join("device/test1-page1.png"),
        src.join("sub/Test 1.png"),
    )
    .unwrap();
    std::fs::write(src.join("readme.txt"), "x").unwrap();
    std::fs::write(src.join(".hidden.note"), "x").unwrap();
    (dir, src)
}

fn opts(out: Option<PathBuf>) -> pipeline::Options {
    pipeline::Options {
        out,
        images: true,
        debug: true,
        force: false,
        keep_template: false,
        allow_unknown_version: false,
    }
}

#[test]
fn converts_folder_with_note_and_png() {
    let (dir, src) = setup();
    let out = dir.path().join("out");
    let (inputs, warnings) = inputs::collect(std::slice::from_ref(&src), None);
    assert_eq!(inputs.len(), 2);
    assert!(warnings.iter().any(|w| w.contains("readme.txt")));
    let outputs = inputs::output_paths(&inputs, Some(&out));
    let mut engine = FakeEngine { calls: 0 };
    for (i, o) in inputs.iter().zip(&outputs) {
        pipeline::convert_one(
            i,
            o,
            &mut engine,
            &opts(Some(out.clone())),
            &FileIndex::default(),
        )
        .unwrap();
    }
    let md = std::fs::read_to_string(out.join("Test 2.md")).unwrap();
    assert!(
        md.starts_with("---\ntype: note\ntitle: \"Test 2\"\n"),
        "{md}"
    );
    assert!(md.contains("\nresource: \"../in/Test 2.note\"\n"), "{md}");
    assert!(md.contains("file_id: \"F20261005225535133622IHP5FqtIJoxD\""));
    assert!(md.contains(", model: \"fake-model\", at: "), "{md}");
    assert!(md.contains("## Page 1\n<!-- page_id: P20261005225535142460xFi05hBOdWD6 -->"));
    assert!(md.contains("![Page 1](Test%202.assets/page-001.png)"));
    assert!(md.contains("Hello page"));
    assert!(md.contains("```mermaid\ngraph LR\nA --> B\n```"));
    assert!(out.join("Test 2.assets/page-001.png").exists());
    assert!(out.join("Test 2.debug.json").exists());
    assert!(out.join("sub/Test 1.md").exists());
    assert_eq!(engine.calls, 2);
}

#[test]
fn rerun_skips_unchanged_and_reconverts_changed() {
    let (dir, src) = setup();
    let out = dir.path().join("out");
    let (inputs, _) = inputs::collect(&[src.join("Test 2.note")], None);
    let outputs = inputs::output_paths(&inputs, Some(&out));
    let mut engine = FakeEngine { calls: 0 };
    pipeline::convert_one(
        &inputs[0],
        &outputs[0],
        &mut engine,
        &opts(Some(out.clone())),
        &FileIndex::default(),
    )
    .unwrap();
    let again = pipeline::convert_one(
        &inputs[0],
        &outputs[0],
        &mut engine,
        &opts(Some(out.clone())),
        &FileIndex::default(),
    )
    .unwrap();
    assert!(matches!(again, pipeline::Outcome::Skipped));
    assert_eq!(engine.calls, 1);
    // A file notedown did not write is never overwritten without --force.
    std::fs::write(&outputs[0], "not frontmatter").unwrap();
    let err = pipeline::convert_one(
        &inputs[0],
        &outputs[0],
        &mut engine,
        &opts(Some(out.clone())),
        &FileIndex::default(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("refusing to overwrite"), "{err}");
    assert_eq!(engine.calls, 1);
    // --force converts again.
    let mut o = opts(Some(out.clone()));
    o.force = true;
    pipeline::convert_one(
        &inputs[0],
        &outputs[0],
        &mut engine,
        &o,
        &FileIndex::default(),
    )
    .unwrap();
    assert_eq!(engine.calls, 2);
}

#[test]
fn same_stem_note_and_png_do_not_collide() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        fixtures().join("notes/test2.note"),
        dir.path().join("X.note"),
    )
    .unwrap();
    std::fs::copy(
        fixtures().join("device/test2-page1.png"),
        dir.path().join("X.png"),
    )
    .unwrap();
    let (inputs, _) = inputs::collect(&[dir.path().to_path_buf()], None);
    let outs = inputs::output_paths(&inputs, None);
    let names: Vec<_> = outs
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert!(
        names.contains(&"X.md".to_string()) && names.contains(&"X.png.md".to_string()),
        "{names:?}"
    );
}

#[test]
fn frontmatter_roundtrip() {
    let fm = Frontmatter {
        resource: "Test 2.note".into(),
        file_id: Some("F1".into()),
        pages: 1,
        keywords: vec![
            "a: b".into(),
            "Übersicht: \"Wäsche\" #1 \\ ok\nzwei\u{1}".into(),
        ],
        links: vec![
            FrontmatterLink {
                page: 1,
                target_file_id: "F2024".into(),
                target_path: "/storage/emulated/0/Note/Bike.note".into(),
                target_md: Some("Über uns/Bike, [x].md".into()),
            },
            FrontmatterLink {
                page: 2,
                target_file_id: "F2025".into(),
                target_path: "/storage/emulated/0/Note/Plan: a, b {c} [d] #1 \"q\".note".into(),
                target_md: None,
            },
        ],
        digests: vec![
            FrontmatterDigest {
                page: 3,
                digest_id: "d1".into(),
                text: "Note: a, b {c} [d] #1 \"q\" \\ Größe\nzwei".into(),
                rect: [256, 796, 271, 88],
            },
            FrontmatterDigest {
                page: 4,
                digest_id: "0123456789abcdef0123456789abcdef".into(),
                text: "plain".into(),
                rect: [0, 1, 2, 3],
            },
        ],
        ..sample_fm()
    };
    let back = Frontmatter::parse(&format!("{}\n## Page 1\n", fm.render())).unwrap();
    assert_eq!(back.source_sha256, "ab");
    assert_eq!(back.keywords, fm.keywords);
    assert_eq!(back.links, fm.links);
    assert_eq!(back.digests, fm.digests);
    assert_eq!(back, fm);
    assert!(fm.render().contains("\\u0001"));
}

#[test]
fn transparent_png_is_flattened_on_white() {
    let mut img = image::RgbaImage::new(10, 10); // fully transparent black
    img.put_pixel(0, 0, image::Rgba([0, 0, 0, 255]));
    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .unwrap();
    let flat = image::load_from_memory(&pipeline::load_png_page(&buf).unwrap())
        .unwrap()
        .to_rgba8();
    assert_eq!(flat.get_pixel(5, 5).0, [255, 255, 255, 255]);
    assert_eq!(flat.get_pixel(0, 0).0, [0, 0, 0, 255]);
}

#[test]
fn never_overwrites_foreign_markdown() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        fixtures().join("notes/test2.note"),
        dir.path().join("X.note"),
    )
    .unwrap();
    std::fs::write(dir.path().join("X.md"), "my own text\n").unwrap();
    let (inputs, _) = inputs::collect(&[dir.path().join("X.note")], None);
    let outs = inputs::output_paths(&inputs, None);
    let mut engine = FakeEngine { calls: 0 };
    let err = pipeline::convert_one(
        &inputs[0],
        &outs[0],
        &mut engine,
        &opts(None),
        &FileIndex::default(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("not created by notedown"), "{err}");
    assert_eq!(std::fs::read_to_string(&outs[0]).unwrap(), "my own text\n");
    assert_eq!(engine.calls, 0);
}

fn fixture_ok(name: &str) -> Option<PathBuf> {
    let p = fixtures().join("local").join(name);
    if p.exists() {
        Some(p)
    } else {
        eprintln!("skipping: local fixture {name} missing");
        None
    }
}

#[test]
fn content_links_resolve_to_converted_bike() {
    let Some(content) = fixture_ok("content.note") else {
        return;
    };
    let Some(bike) = fixture_ok("bike.note") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("in");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::copy(content, src.join("Content.note")).unwrap();
    std::fs::copy(bike, src.join("Bike.note")).unwrap();
    let out = dir.path().join("out");
    let (inputs, _) = inputs::collect(std::slice::from_ref(&src), None);
    let outputs = inputs::output_paths(&inputs, Some(&out));
    let index = FileIndex::build(&inputs, &outputs, Some(&out));
    let mut engine = FakeEngine { calls: 0 };
    for (i, o) in inputs.iter().zip(&outputs) {
        pipeline::convert_one(i, o, &mut engine, &opts(Some(out.clone())), &index).unwrap();
    }
    let md = std::fs::read_to_string(out.join("Content.md")).unwrap();
    assert!(
        md.contains("target_file_id: \"F20240708174639550429bHksqaP2wKQ8\""),
        "{md}"
    );
    assert!(md.contains("[Bike](Bike.md)"), "{md}");
    // Every other file link stays unresolved: `<stem> (<device path>)`. The expected
    // text comes from the private fixture at run time, so no private name is in the repo.
    let nb = notedown_note::Notebook::open(
        &src.join("Content.note"),
        notedown_note::VersionPolicy::AllowUnknown,
    )
    .unwrap();
    let others: Vec<String> = nb
        .links
        .iter()
        .filter(|l| l.kind == notedown_note::LinkKind::File)
        .filter_map(|l| l.target.clone())
        .filter(|t| !t.ends_with("/Bike.note"))
        .collect();
    assert!(!others.is_empty());
    for path in &others {
        let name = path.rsplit('/').next().unwrap();
        let stem = name.strip_suffix(".note").unwrap_or(name);
        let expected = format!(
            "{} ({})",
            notedown_convert::links::escape_md(stem),
            notedown_convert::links::escape_md(path)
        );
        assert!(md.contains(&expected), "{md}");
    }
    assert!(Frontmatter::parse(&md).unwrap().links.len() >= 2);
}

fn link(kind: notedown_note::LinkKind, rect: [u32; 4]) -> notedown_note::Link {
    notedown_note::Link {
        key: String::new(),
        address: 0,
        page: 0,
        position: notedown_note::KeyPosition {
            page: 1,
            top: None,
            left: None,
            height: None,
            width: None,
        },
        kind,
        direction: None,
        target: None,
        target_file_id: None,
        target_page_id: None,
        target_page: None,
        rect,
        rect_original: None,
        timestamp: None,
        style: None,
        protocol: None,
        bitmap_block: None,
        bitmap: None,
        meta: Default::default(),
    }
}

#[test]
fn link_placement_uses_bbox_overlap() {
    use notedown_note::LinkKind;
    use notedown_ocr::OcrBlock;
    let l = link(LinkKind::File, [788, 426, 275, 111]);
    // Same area in 0-1000 coordinates of a 1404x1872 page.
    let hit = OcrBlock {
        label: "Text".into(),
        bbox: Some([561, 227, 757, 287]),
        markdown: "Bike".into(),
    };
    let miss = OcrBlock {
        label: "Text".into(),
        bbox: Some([100, 600, 300, 650]),
        markdown: "Alpha".into(),
    };
    assert_eq!(
        notedown_convert::links::place(&[l], &[miss, hit], 1404, 1872),
        vec![Some(1)]
    );
}

/// Always-running coverage of resolution: a FileIndex from a fake `.md` frontmatter
/// and links applied to synthetic OCR pages.
#[test]
fn apply_links_resolves_by_file_id_without_private_files() {
    use notedown_convert::document::PageOut;
    use notedown_convert::pipeline::PageResult;
    use notedown_note::LinkKind;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir_all(out.join("sub")).unwrap();
    let fm = Frontmatter {
        resource: "Bike.note".into(),
        file_id: Some("FBIKE".into()),
        pages: 1,
        keywords: vec![],
        links: vec![],
        digests: vec![],
        ..sample_fm()
    };
    std::fs::write(out.join("sub/My Bike.md"), fm.render()).unwrap();
    let index = FileIndex::build(&[], &[], Some(&out));
    assert_eq!(
        index.resolve("FBIKE", &out.join("Content.md")).as_deref(),
        Some("sub/My%20Bike.md")
    );
    assert_eq!(
        index.resolve("FBIKE", &out.join("sub/Other.md")).as_deref(),
        Some("My%20Bike.md")
    );
    assert_eq!(index.resolve("FNOPE", &out.join("Content.md")), None);

    let blocks = vec![
        notedown_ocr::OcrBlock {
            label: "Text".into(),
            bbox: Some([561, 227, 757, 287]),
            markdown: "Bike @".into(),
        },
        notedown_ocr::OcrBlock {
            label: "Text".into(),
            bbox: Some([570, 700, 770, 760]),
            markdown: "Bata @".into(),
        },
        notedown_ocr::OcrBlock {
            label: "Text".into(),
            bbox: Some([100, 600, 300, 650]),
            markdown: "Alpha".into(),
        },
    ];
    let parts: Vec<String> = ["Bike @", "Bata @", "Alpha"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut pages = vec![PageResult {
        out: PageOut {
            number: 1,
            page_id: None,
            image_rel: None,
            markdown: parts.join("\n\n"),
            truncated: false,
            empty: false,
        },
        png: None,
        ocr: None,
        blocks,
        parts,
    }];
    let mut l1 = link(LinkKind::File, [788, 426, 275, 111]);
    l1.target = Some("/storage/emulated/0/Note/Bike.note".into());
    l1.target_file_id = Some("FBIKE".into());
    // 0-1000 bbox [570, 700, 770, 760] on a 1404x1872 page
    let mut l2 = link(LinkKind::File, [800, 1310, 281, 112]);
    l2.target = Some("/storage/emulated/0/Note/Beta.note".into());
    l2.target_file_id = Some("FUNKNOWN".into());
    let ctx = notedown_convert::links::LinkContext {
        index: &index,
        from_md: &out.join("Content.md"),
        page_ids: &[None],
        width: 1404,
        height: 1872,
    };
    let fm_links = notedown_convert::links::apply(&[l1, l2], &mut pages, &ctx);
    let md = &pages[0].out.markdown;
    assert!(md.contains("[Bike](sub/My%20Bike.md)"), "{md}");
    assert!(!md.contains("Bike @"), "{md}");
    assert!(
        md.contains("Beta (/storage/emulated/0/Note/Beta.note)"),
        "{md}"
    );
    assert!(!md.contains("Bata"), "{md}");
    assert!(md.contains("Alpha"), "{md}");
    assert!(
        md.contains("Links: [Bike](sub/My%20Bike.md), Beta (/storage/emulated/0/Note/Beta.note)"),
        "{md}"
    );
    assert_eq!(fm_links.len(), 2);
    assert_eq!(fm_links[0].page, 1);
    assert_eq!(fm_links[0].target_file_id, "FBIKE");
    assert_eq!(
        fm_links[0].target_path,
        "/storage/emulated/0/Note/Bike.note"
    );
    assert_eq!(fm_links[0].target_md.as_deref(), Some("sub/My Bike.md"));
    assert_eq!(fm_links[1].target_md, None);
    assert_eq!(
        fm_links[1].target_path,
        "/storage/emulated/0/Note/Beta.note"
    );
}

#[test]
fn convert_without_links_has_no_links_key_and_page_anchor() {
    let (dir, src) = setup();
    let out = dir.path().join("out");
    let (inputs, _) = inputs::collect(&[src.join("Test 2.note")], None);
    let outputs = inputs::output_paths(&inputs, Some(&out));
    let index = FileIndex::build(&inputs, &outputs, Some(&out));
    let mut engine = FakeEngine { calls: 0 };
    pipeline::convert_one(
        &inputs[0],
        &outputs[0],
        &mut engine,
        &opts(Some(out.clone())),
        &index,
    )
    .unwrap();
    let md = std::fs::read_to_string(&outputs[0]).unwrap();
    assert!(!md.contains("links:"), "{md}");
    assert!(md.contains("<a id=\"page-1\"></a>\n## Page 1"), "{md}");
}

#[test]
fn folder_walk_skips_generated_files() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("in");
    std::fs::create_dir_all(src.join("Test 2.assets")).unwrap();
    std::fs::create_dir_all(src.join("out/Test 2.assets")).unwrap();
    std::fs::copy(fixtures().join("notes/test2.note"), src.join("Test 2.note")).unwrap();
    let png = fixtures().join("device/test2-page1.png");
    for rel in [
        "Test 2.assets/page-001.png",
        "Test 2-page-001.png",
        "out/Test 2.assets/page-001.png",
        "out/Other.png",
    ] {
        std::fs::copy(&png, src.join(rel)).unwrap();
    }
    std::fs::copy(
        fixtures().join("notes/test2.note"),
        src.join("out/Copy.note"),
    )
    .unwrap();
    let out = src.join("out");
    let (found, _) = inputs::collect(std::slice::from_ref(&src), Some(&out));
    let names: Vec<_> = found.iter().map(|i| i.rel.clone()).collect();
    assert_eq!(names, vec![PathBuf::from("Test 2.note")]);

    // A page image without its note is a normal input; four-digit pages count as render output.
    std::fs::rename(
        src.join("Test 2-page-001.png"),
        src.join("Lone-page-001.png"),
    )
    .unwrap();
    std::fs::copy(&png, src.join("Test 2-page-1000.png")).unwrap();
    let (found, _) = inputs::collect(std::slice::from_ref(&src), Some(&out));
    let names: Vec<_> = found.iter().map(|i| i.rel.clone()).collect();
    assert_eq!(
        names,
        vec![
            PathBuf::from("Lone-page-001.png"),
            PathBuf::from("Test 2.note")
        ]
    );
}

#[cfg(unix)]
#[test]
fn folder_walk_does_not_follow_symlink_loops() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("in");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::copy(fixtures().join("notes/test2.note"), src.join("sub/A.note")).unwrap();
    std::os::unix::fs::symlink(&src, src.join("sub/loop")).unwrap();
    std::os::unix::fs::symlink(src.join("sub/A.note"), src.join("B.note")).unwrap();
    let (found, warnings) = inputs::collect(std::slice::from_ref(&src), None);
    let names: Vec<_> = found.iter().map(|i| i.rel.clone()).collect();
    assert_eq!(
        names,
        vec![PathBuf::from("B.note"), PathBuf::from("sub/A.note")]
    );
    assert!(warnings.iter().any(|w| w.contains("loop")), "{warnings:?}");
}

fn block(bbox: [u32; 4], text: &str) -> notedown_ocr::OcrBlock {
    notedown_ocr::OcrBlock {
        label: "Text".into(),
        bbox: Some(bbox),
        markdown: text.to_string(),
    }
}

#[test]
fn link_replaces_a_block_only_when_it_covers_half_of_it() {
    use notedown_convert::links::place;
    use notedown_note::LinkKind;
    // A 1000 x 1000 page keeps bbox and pixel coordinates equal.
    let l = link(LinkKind::File, [0, 0, 100, 100]);
    // TOC-like block about the size of the link: replaced
    assert_eq!(
        place(
            std::slice::from_ref(&l),
            &[block([0, 0, 110, 105], "a")],
            1000,
            1000
        ),
        vec![Some(0)]
    );
    // link covers 10000 / 19000 = 0.53 of the block: replaced
    assert_eq!(
        place(
            std::slice::from_ref(&l),
            &[block([0, 0, 100, 190], "a")],
            1000,
            1000
        ),
        vec![Some(0)]
    );
    // link covers 10000 / 22000 = 0.45 of the block: kept
    assert_eq!(
        place(
            std::slice::from_ref(&l),
            &[block([0, 0, 100, 220], "a")],
            1000,
            1000
        ),
        vec![None]
    );
    // large paragraph with a small link inside: kept
    let small = link(LinkKind::File, [400, 400, 60, 30]);
    assert_eq!(
        place(&[small], &[block([50, 100, 950, 700], "a")], 1000, 1000),
        vec![None]
    );
}

#[test]
fn paragraph_with_a_small_link_keeps_its_text() {
    use notedown_convert::document::PageOut;
    use notedown_convert::pipeline::PageResult;
    use notedown_note::LinkKind;
    let text = "A long paragraph that mentions Beta somewhere in the middle.";
    let blocks = vec![block([50, 100, 950, 700], text)];
    let parts = vec![text.to_string()];
    let mut pages = vec![PageResult {
        out: PageOut {
            number: 1,
            page_id: None,
            image_rel: None,
            markdown: parts.join("\n\n"),
            truncated: false,
            empty: false,
        },
        png: None,
        ocr: None,
        blocks,
        parts,
    }];
    let mut l = link(LinkKind::File, [400, 400, 60, 30]);
    l.target = Some("/storage/emulated/0/Note/Beta.note".into());
    l.target_file_id = Some("FBETA".into());
    let index = FileIndex::default();
    let ctx = notedown_convert::links::LinkContext {
        index: &index,
        from_md: std::path::Path::new("Alpha.md"),
        page_ids: &[None],
        width: 1000,
        height: 1000,
    };
    notedown_convert::links::apply(&[l], &mut pages, &ctx);
    let md = &pages[0].out.markdown;
    assert!(md.starts_with(text), "{md}");
    assert!(
        md.ends_with("Links: Beta (/storage/emulated/0/Note/Beta.note)\n"),
        "{md}"
    );
}

#[test]
fn note_strings_in_the_body_are_escaped() {
    use notedown_convert::document::PageOut;
    use notedown_convert::pipeline::PageResult;
    use notedown_note::LinkKind;
    let mut pages = vec![PageResult {
        out: PageOut {
            number: 1,
            page_id: None,
            image_rel: None,
            markdown: "Text".into(),
            truncated: false,
            empty: false,
        },
        png: None,
        ocr: None,
        blocks: vec![],
        parts: vec!["Text".into()],
    }];
    let mut l = link(LinkKind::File, [0, 0, 10, 10]);
    l.target = Some("/storage/emulated/0/Note/a_b [x] (1) *`<i>.note".into());
    l.target_file_id = Some("FX".into());
    let index = FileIndex::default();
    let ctx = notedown_convert::links::LinkContext {
        index: &index,
        from_md: std::path::Path::new("Alpha.md"),
        page_ids: &[None],
        width: 1000,
        height: 1000,
    };
    notedown_convert::links::apply(&[l], &mut pages, &ctx);
    let md = &pages[0].out.markdown;
    let stem = r"a\_b \[x\] \(1\) \*\`\<i>";
    assert!(
        md.contains(&format!(
            "Links: {stem} (/storage/emulated/0/Note/{stem}.note)"
        )),
        "{md}"
    );
}

#[test]
fn page_id_cannot_close_its_comment() {
    use notedown_convert::document::{PageOut, render_document};
    let fm = Frontmatter {
        resource: "A.note".into(),
        file_id: None,
        pages: 1,
        keywords: vec![],
        links: vec![],
        digests: vec![],
        ..sample_fm()
    };
    let page = PageOut {
        number: 1,
        page_id: Some("P2026-x --> <script>---".into()),
        image_rel: None,
        markdown: "Hi".into(),
        truncated: false,
        empty: false,
    };
    let doc = render_document(&fm, &[page]);
    assert!(doc.contains("<!-- page_id: P2026-x-script- -->\n"), "{doc}");
    assert_eq!(doc.matches("-->").count(), 1, "{doc}");
    assert!(!doc.contains("<script"), "{doc}");
}

struct BoxSource {
    fail_page: Option<usize>,
}
impl pipeline::PageSource for BoxSource {
    fn pages(&self) -> usize {
        2
    }
    fn is_blank(&self, _i: usize) -> anyhow::Result<bool> {
        Ok(false)
    }
    fn png(&self, _i: usize) -> anyhow::Result<Vec<u8>> {
        Ok(vec![0])
    }
    fn page_id(&self, _i: usize) -> Option<String> {
        None
    }
    fn text_boxes(&self, i: usize) -> anyhow::Result<Vec<notedown_note::TextBox>> {
        if self.fail_page == Some(i) {
            anyhow::bail!("broken record");
        }
        let mk = |digest: Option<notedown_note::DigestRef>| notedown_note::TextBox {
            page: i,
            record_index: 0,
            id: 61,
            rect: [256, 796, 271, 88],
            text: "Text: with #".into(),
            font_size: Some(48.0),
            font_path: None,
            created: None,
            style_raw: String::new(),
            flags_raw: vec![],
            digest,
            source_stroke_ids: vec![53],
            source_strokes: vec![],
        };
        Ok(if i == 0 {
            vec![
                mk(None),
                mk(Some(notedown_note::DigestRef {
                    id: "abc".into(),
                    time_ms: Some(1),
                    source: None,
                })),
            ]
        } else {
            vec![mk(None)]
        })
    }
}

fn fm_with(digests: Vec<FrontmatterDigest>) -> Frontmatter {
    Frontmatter {
        resource: "a.note".into(),
        file_id: None,
        pages: 2,
        keywords: vec![],
        links: vec![],
        digests,
        ..sample_fm()
    }
}

#[test]
fn digests_are_listed_in_the_frontmatter_and_body_is_unchanged() {
    let src = BoxSource { fail_page: None };
    let mut warnings = Vec::new();
    let (boxes, digests) = pipeline::collect_text_boxes(&src, &mut |w| warnings.push(w));
    assert!(warnings.is_empty());
    assert_eq!(boxes.len(), 2);
    assert_eq!(digests.len(), 1, "only text boxes with a digest");
    assert_eq!(digests[0].page, 1);
    let mut engine = FakeEngine { calls: 0 };
    let results = pipeline::process_pages(&src, &mut engine, false, "x.assets").unwrap();
    let outs: Vec<_> = results.iter().map(|r| r.out.clone()).collect();
    let with = notedown_convert::document::render_document(&fm_with(digests), &outs);
    let without = notedown_convert::document::render_document(&fm_with(vec![]), &outs);
    assert!(
        with.contains("digests:\n  - { page: 1, digest_id: \"abc\","),
        "{with}"
    );
    assert!(!without.contains("digests:"));
    let body = |d: &str| d.split_once("\n---\n").unwrap().1.to_string();
    assert_eq!(
        body(&with).split_once("\n---\n").map(|x| x.1),
        body(&without).split_once("\n---\n").map(|x| x.1)
    );
    assert_eq!(Frontmatter::parse(&with).unwrap().digests.len(), 1);
}

#[test]
fn unreadable_text_boxes_warn_and_skip_the_page() {
    let src = BoxSource { fail_page: Some(0) };
    let mut warnings = Vec::new();
    let (boxes, digests) = pipeline::collect_text_boxes(&src, &mut |w| warnings.push(w));
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].starts_with("page 1:"), "{warnings:?}");
    assert!(digests.is_empty());
    assert_eq!(boxes.len(), 2);
}

#[test]
fn bike_digest_lists_one_digest_on_page_3() {
    let Some(note) = fixture_ok("bike-digest.note") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("in");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::copy(note, src.join("Bike digest.note")).unwrap();
    let out = dir.path().join("out");
    let (inputs, _) = inputs::collect(std::slice::from_ref(&src), None);
    let outputs = inputs::output_paths(&inputs, Some(&out));
    let index = FileIndex::build(&inputs, &outputs, Some(&out));
    let mut engine = FakeEngine { calls: 0 };
    for (i, o) in inputs.iter().zip(&outputs) {
        pipeline::convert_one(i, o, &mut engine, &opts(Some(out.clone())), &index).unwrap();
    }
    let md = std::fs::read_to_string(out.join("Bike digest.md")).unwrap();
    let fm = Frontmatter::parse(&md).unwrap();
    assert_eq!(fm.digests.len(), 1, "{md}");
    assert_eq!(fm.digests[0].page, 3);
    assert_eq!(fm.digests[0].rect, [256, 796, 271, 88]);
    let debug = std::fs::read_to_string(out.join("Bike digest.debug.json")).unwrap();
    assert!(debug.contains("\"text_boxes\""));
    assert!(debug.contains("\"source_stroke_count\": 7"));
}

fn sample_fm() -> Frontmatter {
    Frontmatter {
        title: "Test 2".into(),
        description: "Handwritten Supernote note, 1 page, read by OCR.".into(),
        resource: "Test 2.note".into(),
        generated_by: "notedown/0.1.0".into(),
        generated_model: "pagevision qwen3.5-4b Q4_K_M".into(),
        generated_at: "2026-10-05T23:10:00Z".into(),
        source_date: Some("2026-10-05".into()),
        last_modified: Some("2026-10-05T22:55:35Z".into()),
        status: "stable".into(),
        source_sha256: "ab".into(),
        file_id: Some("F20261005225535133622IHP5FqtIJoxD".into()),
        pages: 1,
        keywords: vec![],
        links: vec![],
        digests: vec![],
        extra: vec![],
        format: HeaderFormat::Okf,
        crlf: false,
    }
}

#[test]
fn renders_the_open_knowledge_format_header_of_a_note() {
    let fm = Frontmatter {
        links: vec![FrontmatterLink {
            page: 1,
            target_file_id: "F20240708174639550429bHksqaP2wKQ8".into(),
            target_path: "/storage/emulated/0/Note/Bike.note".into(),
            target_md: Some("Bike.md".into()),
        }],
        digests: vec![FrontmatterDigest {
            page: 1,
            digest_id: "d1".into(),
            text: "plain".into(),
            rect: [256, 796, 271, 88],
        }],
        ..sample_fm()
    };
    assert_eq!(
        fm.render(),
        "---
type: note
title: \"Test 2\"
description: \"Handwritten Supernote note, 1 page, read by OCR.\"
resource: \"Test 2.note\"
tags: [supernote]
generated: { by: notedown/0.1.0, model: \"pagevision qwen3.5-4b Q4_K_M\", at: 2026-10-05T23:10:00Z }
source_date: 2026-10-05
sources:
  - { id: note, resource: \"Test 2.note\", last_modified: 2026-10-05T22:55:35Z }
status: stable
source_sha256: \"ab\"
file_id: \"F20261005225535133622IHP5FqtIJoxD\"
pages: 1
keywords: []
links:
  - { page: 1, target_file_id: \"F20240708174639550429bHksqaP2wKQ8\", target_path: \"/storage/emulated/0/Note/Bike.note\", target_md: \"Bike.md\" }
digests:
  - { page: 1, digest_id: \"d1\", text: \"plain\", rect: \"256,796,271,88\" }
---
"
    );
}

#[test]
fn renders_the_open_knowledge_format_header_of_a_png_page() {
    let fm = Frontmatter {
        title: "Test 1".into(),
        description: "Handwritten Supernote page, read by OCR.".into(),
        resource: "../in/sub/Test 1.png".into(),
        generated_model: "fake: model, v2".into(),
        status: "draft".into(),
        file_id: None,
        ..sample_fm()
    };
    assert_eq!(
        fm.render(),
        "---
type: note
title: \"Test 1\"
description: \"Handwritten Supernote page, read by OCR.\"
resource: \"../in/sub/Test 1.png\"
tags: [supernote]
generated: { by: notedown/0.1.0, model: \"fake: model, v2\", at: 2026-10-05T23:10:00Z }
source_date: 2026-10-05
sources:
  - { id: note, resource: \"../in/sub/Test 1.png\", last_modified: 2026-10-05T22:55:35Z }
status: draft
source_sha256: \"ab\"
pages: 1
keywords: []
---
"
    );
    assert_eq!(Frontmatter::parse(&fm.render()), Some(fm));
}

#[test]
fn hostile_values_are_quoted_in_the_header() {
    let fm = Frontmatter {
        title: "a: b #c".into(),
        resource: "x, {y}: z.note".into(),
        generated_at: "2026-10-05T23:10:00 #".into(),
        ..sample_fm()
    };
    let text = fm.render();
    assert!(text.contains("title: \"a: b #c\"\n"), "{text}");
    assert!(text.contains("resource: \"x, {y}: z.note\"\n"), "{text}");
    assert!(
        text.contains("- { id: note, resource: \"x, {y}: z.note\", last_modified:"),
        "{text}"
    );
    assert!(text.contains("at: \"2026-10-05T23:10:00 #\" }"), "{text}");
    assert_eq!(Frontmatter::parse(&text), Some(fm));
}

#[test]
fn parses_the_header_notedown_wrote_before() {
    let old = "---
source: sub/Test 2.note
source_sha256: ab
kind: supernote-note
file_id: F1
pages: 2
model: pagevision qwen3.5-4b Q4_K_M
converted_at: \"2026-10-05T23:10:00Z\"
generator: notedown 0.1.0
keywords:
  - \"p1: Einkauf\"
links:
  - { page: 1, target_file_id: F2, target_path: /storage/emulated/0/Note/Bike.note, target_md: Bike.md }
---

<a id=\"page-1\"></a>
## Page 1
";
    let fm = Frontmatter::parse(old).unwrap();
    assert_eq!(fm.format, HeaderFormat::Old);
    assert_eq!(fm.generated_by, "notedown/0.1.0");
    assert_eq!(fm.generated_model, "pagevision qwen3.5-4b Q4_K_M");
    assert_eq!(fm.generated_at, "2026-10-05T23:10:00Z");
    assert_eq!(fm.source_sha256, "ab");
    assert_eq!(fm.file_id.as_deref(), Some("F1"));
    assert_eq!(fm.pages, 2);
    assert_eq!(fm.keywords, vec!["p1: Einkauf".to_string()]);
    assert_eq!(fm.links.len(), 1);
    assert_eq!(fm.title, "Test 2");
    // a header with both `generator` and `generated` is not one notedown wrote
    let mixed = old.replace(
        "generator: notedown 0.1.0\n",
        "generator: notedown 0.1.0\ngenerated: { by: notedown/0.1.0, at: x }\n",
    );
    assert_eq!(Frontmatter::parse(&mixed), None);
}

#[test]
fn new_header_round_trips() {
    let fm = sample_fm();
    let back = Frontmatter::parse(&format!("{}\n## Page 1\n", fm.render())).unwrap();
    assert_eq!(back.format, HeaderFormat::Okf);
    assert_eq!(back, fm);
}

/// Writes `<dir>/Test 2.md` in the format notedown used before OKF, for the note
/// `<dir>/../in/Test 2.note` (or any note), with the given model.
fn old_header_md(note: &std::path::Path, model: &str, body: &str) -> String {
    use sha2::Digest;
    let sha = const_hex::encode(sha2::Sha256::digest(std::fs::read(note).unwrap()));
    format!(
        "---
source: Test 2.note
source_sha256: {sha}
kind: supernote-note
file_id: F20261005225535133622IHP5FqtIJoxD
pages: 1
model: {model}
converted_at: \"2026-10-01T08:00:00Z\"
generator: notedown {}
keywords: []
---
{body}",
        env!("CARGO_PKG_VERSION")
    )
}

const OLD_BODY: &str = "
<a id=\"page-1\"></a>
## Page 1
<!-- page_id: P20261005225535142460xFi05hBOdWD6 -->
<!-- notedown: truncated -->

Old OCR text, kept as it is: a, b #c
";

fn pin_mtime(path: &std::path::Path) -> time::OffsetDateTime {
    // 2026-10-05T22:55:35Z
    let at = time::OffsetDateTime::from_unix_timestamp(1_791_240_935).unwrap();
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(at.into())
        .unwrap();
    at
}

fn local_date(at: time::OffsetDateTime) -> String {
    let offset = notedown_cloud::localtime::local_utc_offset().unwrap_or(time::UtcOffset::UTC);
    let d = at.to_offset(offset).date();
    format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())
}

#[test]
fn old_header_is_updated_without_ocr_and_the_body_stays() {
    let (dir, src) = setup();
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let note = src.join("Test 2.note");
    let at = pin_mtime(&note);
    let md_path = out.join("Test 2.md");
    std::fs::write(&md_path, old_header_md(&note, "fake-model", OLD_BODY)).unwrap();
    let (inputs, _) = inputs::collect(std::slice::from_ref(&note), None);
    let outputs = inputs::output_paths(&inputs, Some(&out));
    assert_eq!(outputs[0], md_path);
    let mut engine = FakeEngine { calls: 0 };
    let outcome = pipeline::convert_one(
        &inputs[0],
        &outputs[0],
        &mut engine,
        &opts(Some(out.clone())),
        &FileIndex::default(),
    )
    .unwrap();
    assert_eq!(outcome, pipeline::Outcome::HeaderUpdated);
    assert_eq!(engine.calls, 0);
    let md = std::fs::read_to_string(&md_path).unwrap();
    let (head, body) = md.split_at(md.find("\n---\n").unwrap() + 5);
    assert_eq!(body, OLD_BODY, "the body is kept byte for byte");
    let fm = Frontmatter::parse(head).unwrap();
    assert_eq!(fm.format, HeaderFormat::Okf);
    assert_eq!(fm.title, "Test 2");
    assert_eq!(fm.resource, "../in/Test 2.note");
    assert_eq!(
        fm.description,
        "Handwritten Supernote note, 1 page, read by OCR."
    );
    assert_eq!(
        fm.generated_by,
        format!("notedown/{}", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(fm.generated_model, "fake-model");
    assert_eq!(fm.generated_at, "2026-10-01T08:00:00Z");
    assert_eq!(fm.last_modified.as_deref(), Some("2026-10-05T22:55:35Z"));
    assert_eq!(fm.source_date, Some(local_date(at)));
    assert_eq!(fm.status, "draft", "the old body has a truncated page");
    assert_eq!(
        fm.file_id.as_deref(),
        Some("F20261005225535133622IHP5FqtIJoxD")
    );
    // the next run finds it current
    let again = pipeline::convert_one(
        &inputs[0],
        &outputs[0],
        &mut engine,
        &opts(Some(out.clone())),
        &FileIndex::default(),
    )
    .unwrap();
    assert_eq!(again, pipeline::Outcome::Skipped);
    assert_eq!(engine.calls, 0);
}

#[test]
fn old_header_of_another_model_is_converted_again() {
    let (_dir, src) = setup();
    let note = src.join("Test 2.note");
    let md_path = src.join("Test 2.md");
    std::fs::write(&md_path, old_header_md(&note, "other-model", OLD_BODY)).unwrap();
    let (inputs, _) = inputs::collect(std::slice::from_ref(&note), None);
    let outputs = inputs::output_paths(&inputs, None);
    let mut engine = FakeEngine { calls: 0 };
    let outcome = pipeline::convert_one(
        &inputs[0],
        &outputs[0],
        &mut engine,
        &opts(None),
        &FileIndex::default(),
    )
    .unwrap();
    assert_eq!(outcome, pipeline::Outcome::Converted { pages: 1 });
    assert_eq!(engine.calls, 1, "an old notedown file may be overwritten");
    let md = std::fs::read_to_string(&md_path).unwrap();
    assert!(md.contains("\nresource: \"Test 2.note\"\n"), "{md}");
    assert!(md.contains("\nstatus: stable\n"), "{md}");
    assert!(!md.contains("Old OCR text"), "{md}");
}

#[test]
fn reserved_names_get_the_source_extension() {
    let input = |name: &str, kind| inputs::Input {
        source: PathBuf::from("/n").join(name),
        kind,
        rel: PathBuf::from(name),
    };
    let found = vec![
        input("index.note", inputs::InputKind::Note),
        input("Log.note", inputs::InputKind::Note),
        input("INDEX.png", inputs::InputKind::Png),
        input("sub/log.png", inputs::InputKind::Png),
        input("sub/log.note", inputs::InputKind::Note),
        input("Logbook.note", inputs::InputKind::Note),
    ];
    let names = |outs: Vec<PathBuf>| -> Vec<String> {
        outs.iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect()
    };
    assert_eq!(
        names(inputs::output_paths(&found, None)),
        vec![
            "/n/index.note.md",
            "/n/Log.note.md",
            "/n/INDEX.png.md",
            "/n/sub/log.png.md",
            "/n/sub/log.note.md",
            "/n/Logbook.md",
        ]
    );
    assert_eq!(
        names(inputs::output_paths(
            &found,
            Some(std::path::Path::new("/o"))
        )),
        vec![
            "/o/index.note.md",
            "/o/Log.note.md",
            "/o/INDEX.png.md",
            "/o/sub/log.png.md",
            "/o/sub/log.note.md",
            "/o/Logbook.md",
        ]
    );
}

#[test]
fn resource_is_relative_to_the_markdown_file() {
    let (dir, src) = setup();
    let mut engine = FakeEngine { calls: 0 };
    // next to the sources
    let (found, _) = inputs::collect(std::slice::from_ref(&src), None);
    let outputs = inputs::output_paths(&found, None);
    for (i, o) in found.iter().zip(&outputs) {
        pipeline::convert_one(i, o, &mut engine, &opts(None), &FileIndex::default()).unwrap();
    }
    let read = |p: PathBuf| Frontmatter::parse(&std::fs::read_to_string(p).unwrap()).unwrap();
    assert_eq!(read(src.join("Test 2.md")).resource, "Test 2.note");
    assert_eq!(read(src.join("sub/Test 1.md")).resource, "Test 1.png");
    // under --out, written with a `..` in the path
    let out = src.join("sub/../../out");
    let outputs = inputs::output_paths(&found, Some(&out));
    for (i, o) in found.iter().zip(&outputs) {
        pipeline::convert_one(
            i,
            o,
            &mut engine,
            &opts(Some(out.clone())),
            &FileIndex::default(),
        )
        .unwrap();
    }
    let out = dir.path().join("out");
    let note = read(out.join("Test 2.md"));
    assert_eq!(note.resource, "../in/Test 2.note");
    assert_eq!(
        note.description,
        "Handwritten Supernote note, 1 page, read by OCR."
    );
    let png = read(out.join("sub/Test 1.md"));
    assert_eq!(png.resource, "../../in/sub/Test 1.png");
    assert_eq!(png.description, "Handwritten Supernote page, read by OCR.");
    assert_eq!(png.title, "Test 1");
    assert_eq!(png.file_id, None);
}

struct TruncatingEngine;
impl OcrEngine for TruncatingEngine {
    fn ocr_png(&mut self, _png: &[u8]) -> Result<PageOcr, OcrError> {
        Ok(PageOcr {
            prompt_tokens: 1,
            output_tokens: 1,
            truncated: true,
            ..PageOcr::from_blocks(vec![notedown_ocr::OcrBlock {
                label: "Text".into(),
                bbox: None,
                markdown: "Cut".into(),
            }])
        })
    }
    fn label(&self) -> &str {
        "fake-model"
    }
}

#[test]
fn a_truncated_page_makes_the_note_a_draft() {
    let (dir, src) = setup();
    let note = src.join("Test 2.note");
    let at = pin_mtime(&note);
    let (found, _) = inputs::collect(std::slice::from_ref(&note), None);
    let out = dir.path().join("out");
    let outputs = inputs::output_paths(&found, Some(&out));
    pipeline::convert_one(
        &found[0],
        &outputs[0],
        &mut TruncatingEngine,
        &opts(Some(out.clone())),
        &FileIndex::default(),
    )
    .unwrap();
    let md = std::fs::read_to_string(&outputs[0]).unwrap();
    assert!(md.contains("<!-- notedown: truncated -->"), "{md}");
    let fm = Frontmatter::parse(&md).unwrap();
    assert_eq!(fm.status, "draft");
    assert_eq!(fm.last_modified.as_deref(), Some("2026-10-05T22:55:35Z"));
    assert_eq!(fm.source_date, Some(local_date(at)));
    assert!(fm.generated_at.ends_with('Z'), "{}", fm.generated_at);
}

/// OKF v0.2 conformance (section 11) without a YAML parser: every `.md` that is not
/// `index.md` or `log.md` opens with a frontmatter block of `key: value` lines and list
/// items, and has a non-empty `type`.
fn assert_okf_bundle(root: &std::path::Path) {
    fn walk(dir: &std::path::Path, found: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, found);
            } else if p.extension().is_some_and(|x| x == "md") {
                found.push(p);
            }
        }
    }
    let mut found = Vec::new();
    walk(root, &mut found);
    assert!(!found.is_empty());
    for p in found {
        let name = p.file_name().unwrap().to_string_lossy().to_lowercase();
        assert!(name != "index.md" && name != "log.md", "{}", p.display());
        let text = std::fs::read_to_string(&p).unwrap();
        let block = text
            .strip_prefix("---\n")
            .and_then(|t| t.split_once("\n---\n"))
            .map(|(b, _)| b)
            .unwrap_or_else(|| panic!("no frontmatter in {}", p.display()));
        let mut kind = None;
        for line in block.lines() {
            if line.starts_with("  - ") {
                continue;
            }
            let (key, value) = line
                .split_once(':')
                .unwrap_or_else(|| panic!("{}: bad line {line:?}", p.display()));
            assert!(
                !key.is_empty()
                    && key
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{}: bad key {key:?}",
                p.display()
            );
            if key == "type" {
                kind = Some(value.trim().to_string());
            }
        }
        assert!(
            kind.is_some_and(|k| !k.is_empty()),
            "{}: no type",
            p.display()
        );
    }
}

#[test]
fn every_written_file_is_an_okf_concept() {
    let (dir, src) = setup();
    std::fs::copy(fixtures().join("notes/test2.note"), src.join("index.note")).unwrap();
    std::fs::copy(
        fixtures().join("device/test2-page1.png"),
        src.join("sub/Log.png"),
    )
    .unwrap();
    let out = dir.path().join("out");
    let (found, _) = inputs::collect(std::slice::from_ref(&src), None);
    let outputs = inputs::output_paths(&found, Some(&out));
    let index = FileIndex::build(&found, &outputs, Some(&out));
    let mut engine = FakeEngine { calls: 0 };
    for (i, o) in found.iter().zip(&outputs) {
        pipeline::convert_one(i, o, &mut engine, &opts(Some(out.clone())), &index).unwrap();
    }
    assert!(out.join("index.note.md").exists());
    assert!(out.join("sub/Log.png.md").exists());
    assert_okf_bundle(&out);
}

#[test]
fn file_index_reads_the_file_id_of_an_old_header() {
    let (dir, src) = setup();
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(
        out.join("Old.md"),
        old_header_md(&src.join("Test 2.note"), "fake-model", OLD_BODY),
    )
    .unwrap();
    let index = FileIndex::build(&[], &[], Some(&out));
    assert_eq!(
        index.path_of("F20261005225535133622IHP5FqtIJoxD"),
        Some(out.join("Old.md").as_path())
    );
}

#[test]
fn out_folder_inside_the_source_folder_points_up() {
    let (_dir, src) = setup();
    let note = src.join("Test 2.note");
    let out = src.join("md");
    let (found, _) = inputs::collect(std::slice::from_ref(&note), Some(&out));
    let outputs = inputs::output_paths(&found, Some(&out));
    let mut engine = FakeEngine { calls: 0 };
    pipeline::convert_one(
        &found[0],
        &outputs[0],
        &mut engine,
        &opts(Some(out.clone())),
        &FileIndex::default(),
    )
    .unwrap();
    let md = std::fs::read_to_string(out.join("Test 2.md")).unwrap();
    assert_eq!(Frontmatter::parse(&md).unwrap().resource, "../Test 2.note");
}

#[test]
fn control_and_separator_characters_are_escaped() {
    for (c, code) in [
        ('\u{1}', "\\u0001"),
        ('\u{7f}', "\\u007F"),
        ('\u{80}', "\\u0080"),
        ('\u{85}', "\\u0085"),
        ('\u{9f}', "\\u009F"),
        ('\u{2028}', "\\u2028"),
        ('\u{2029}', "\\u2029"),
        ('\u{fffe}', "\\uFFFE"),
        ('\u{ffff}', "\\uFFFF"),
    ] {
        let v = format!("a{c}b");
        let fm = Frontmatter {
            title: v.clone(),
            resource: format!("{v}.note"),
            generated_model: v.clone(),
            keywords: vec![v.clone()],
            digests: vec![FrontmatterDigest {
                page: 1,
                digest_id: v.clone(),
                text: v.clone(),
                rect: [1, 2, 3, 4],
            }],
            ..sample_fm()
        };
        let text = fm.render();
        assert!(!text.contains(c), "{code} raw in {text:?}");
        assert!(text.contains(&format!("title: \"a{code}b\"\n")), "{text}");
        assert!(text.contains(&format!("model: \"a{code}b\"")), "{text}");
        assert_eq!(Frontmatter::parse(&text), Some(fm), "{code}");
    }
}

#[test]
fn values_that_look_like_other_types_are_quoted() {
    for v in [
        "0x1F",
        "0o17",
        ".inf",
        "20231015_143012",
        "2026-10-05",
        "2026-10-05T23:10:00Z",
        "yes",
        "2024",
        "~",
    ] {
        let fm = Frontmatter {
            title: v.into(),
            keywords: vec![v.into()],
            file_id: Some(v.into()),
            ..sample_fm()
        };
        let text = fm.render();
        assert!(text.contains(&format!("title: \"{v}\"\n")), "{text}");
        assert!(
            text.contains(&format!("keywords:\n  - \"{v}\"\n")),
            "{text}"
        );
        assert!(text.contains(&format!("file_id: \"{v}\"\n")), "{text}");
        assert_eq!(Frontmatter::parse(&text), Some(fm));
    }
    // dates and times are plain only when they are real
    let fm = Frontmatter {
        generated_at: "2026-13-45T99:99:99Z".into(),
        source_date: Some("2026-02-30".into()),
        last_modified: Some("2026-10-05T22:55:35+02:00".into()),
        ..sample_fm()
    };
    let text = fm.render();
    assert!(text.contains("at: \"2026-13-45T99:99:99Z\" }"), "{text}");
    assert!(text.contains("source_date: \"2026-02-30\"\n"), "{text}");
    assert!(
        text.contains("last_modified: 2026-10-05T22:55:35+02:00 }"),
        "{text}"
    );
    assert_eq!(Frontmatter::parse(&text), Some(fm));
}

#[test]
fn unknown_source_times_are_left_out() {
    let fm = Frontmatter {
        source_date: None,
        last_modified: None,
        ..sample_fm()
    };
    let text = fm.render();
    assert!(!text.contains("source_date"), "{text}");
    assert!(
        text.contains("sources:\n  - { id: note, resource: \"Test 2.note\" }\n"),
        "{text}"
    );
    assert_eq!(Frontmatter::parse(&text), Some(fm));
}

#[cfg(unix)]
#[test]
fn resource_follows_a_symlinked_out_folder() {
    let (dir, src) = setup();
    let real = dir.path().join("deep/real");
    std::fs::create_dir_all(&real).unwrap();
    let link = dir.path().join("out-link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let note = src.join("Test 2.note");
    let (found, _) = inputs::collect(std::slice::from_ref(&note), Some(&link));
    let outputs = inputs::output_paths(&found, Some(&link));
    let mut engine = FakeEngine { calls: 0 };
    pipeline::convert_one(
        &found[0],
        &outputs[0],
        &mut engine,
        &opts(Some(link.clone())),
        &FileIndex::default(),
    )
    .unwrap();
    let md = std::fs::read_to_string(real.join("Test 2.md")).unwrap();
    assert_eq!(
        Frontmatter::parse(&md).unwrap().resource,
        "../../in/Test 2.note"
    );
}

fn convert_note(
    note: &std::path::Path,
    out: &std::path::Path,
    engine: &mut FakeEngine,
) -> pipeline::Outcome {
    let (found, _) = inputs::collect(&[note.to_path_buf()], None);
    let outputs = inputs::output_paths(&found, Some(out));
    pipeline::convert_one(
        &found[0],
        &outputs[0],
        engine,
        &opts(Some(out.to_path_buf())),
        &FileIndex::default(),
    )
    .unwrap()
}

#[test]
fn stale_source_fields_get_a_header_update() {
    let (dir, src) = setup();
    let out = dir.path().join("out");
    let note = src.join("Test 2.note");
    let mut engine = FakeEngine { calls: 0 };
    assert_eq!(
        convert_note(&note, &out, &mut engine),
        pipeline::Outcome::Converted { pages: 1 }
    );
    let md_path = out.join("Test 2.md");
    let body = |p: &std::path::Path| {
        let t = std::fs::read_to_string(p).unwrap();
        notedown_convert::document::body_of(&t).unwrap().to_string()
    };
    let first_body = body(&md_path);
    // only the time of the source changed
    let at = pin_mtime(&note);
    assert_eq!(
        convert_note(&note, &out, &mut engine),
        pipeline::Outcome::HeaderUpdated
    );
    let fm = Frontmatter::parse(&std::fs::read_to_string(&md_path).unwrap()).unwrap();
    assert_eq!(fm.last_modified.as_deref(), Some("2026-10-05T22:55:35Z"));
    assert_eq!(fm.source_date, Some(local_date(at)));
    // the source folder moved
    let moved = dir.path().join("moved");
    std::fs::rename(&src, &moved).unwrap();
    assert_eq!(
        convert_note(&moved.join("Test 2.note"), &out, &mut engine),
        pipeline::Outcome::HeaderUpdated
    );
    let fm = Frontmatter::parse(&std::fs::read_to_string(&md_path).unwrap()).unwrap();
    assert_eq!(fm.resource, "../moved/Test 2.note");
    assert_eq!(body(&md_path), first_body);
    assert_eq!(
        convert_note(&moved.join("Test 2.note"), &out, &mut engine),
        pipeline::Outcome::Skipped
    );
    assert_eq!(engine.calls, 1, "OCR ran only once");
}

/// The header as Crystalline's `emit_engram` writes a notedown header back: other key
/// order, block lists, single quotes, `+00:00`, plus a key a person added.
fn crystalline_form(note: &std::path::Path, at: time::OffsetDateTime) -> String {
    use sha2::Digest;
    let sha = const_hex::encode(sha2::Sha256::digest(std::fs::read(note).unwrap()));
    format!(
        "---
type: note
title: Test 2
description: Handwritten Supernote note, 1 page, read by OCR.
tags:
- supernote
status: stable
source_date: {}
resource: ../in/Test 2.note
generated: {{ by: notedown/{}, model: \"fake-model\", at: 2026-10-05T23:10:00+00:00 }}
sources:
- id: note
  resource: ../in/Test 2.note
  last_modified: 2026-10-05T22:55:35+00:00
source_sha256: {sha}
file_id: F20261005225535133622IHP5FqtIJoxD
pages: 1
keywords:
- 'p1: it''s'
links:
- page: 1
  target_file_id: F2
  target_path: /storage/emulated/0/Note/Bike.note
  target_md: Bike.md
digests:
- page: 1
  digest_id: d1
  text: plain
  rect: 1,2,3,4
verified:
- by: human:jordi
  at: 2026-10-06T09:00:00+02:00
---

<a id=\"page-1\"></a>
## Page 1

Text checked by a person.
",
        local_date(at),
        env!("CARGO_PKG_VERSION")
    )
}

#[test]
fn a_header_written_back_by_crystalline_stays_notedowns() {
    let (dir, src) = setup();
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let note = src.join("Test 2.note");
    let at = pin_mtime(&note);
    let md_path = out.join("Test 2.md");
    let text = crystalline_form(&note, at);
    std::fs::write(&md_path, &text).unwrap();
    let fm = Frontmatter::parse(&text).unwrap();
    assert_eq!(fm.keywords, vec!["p1: it's".to_string()]);
    assert_eq!(fm.links[0].target_md.as_deref(), Some("Bike.md"));
    assert_eq!(fm.digests[0].rect, [1, 2, 3, 4]);
    let mut engine = FakeEngine { calls: 0 };
    assert_eq!(
        convert_note(&note, &out, &mut engine),
        pipeline::Outcome::Skipped
    );
    assert_eq!(std::fs::read_to_string(&md_path).unwrap(), text);
    // the source changes: converted again, the added key stays
    std::fs::copy(fixtures().join("notes/test1.note"), &note).unwrap();
    assert!(matches!(
        convert_note(&note, &out, &mut engine),
        pipeline::Outcome::Converted { .. }
    ));
    assert_eq!(engine.calls, 1);
    let md = std::fs::read_to_string(&md_path).unwrap();
    assert!(
        md.contains("\nverified:\n- by: human:jordi\n  at: 2026-10-06T09:00:00+02:00\n---\n"),
        "{md}"
    );
    assert_eq!(
        Frontmatter::parse(&md).unwrap().extra,
        vec!["verified:\n- by: human:jordi\n  at: 2026-10-06T09:00:00+02:00\n".to_string()]
    );
    assert!(!md.contains("Text checked by a person"), "{md}");
}

#[test]
fn a_header_only_update_keeps_added_keys() {
    let (dir, src) = setup();
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let note = src.join("Test 2.note");
    let at = pin_mtime(&note);
    let md_path = out.join("Test 2.md");
    let text = crystalline_form(&note, at).replace("resource: ../in/", "resource: ../old/");
    std::fs::write(&md_path, &text).unwrap();
    let mut engine = FakeEngine { calls: 0 };
    assert_eq!(
        convert_note(&note, &out, &mut engine),
        pipeline::Outcome::HeaderUpdated
    );
    let md = std::fs::read_to_string(&md_path).unwrap();
    assert!(md.contains("\nresource: \"../in/Test 2.note\"\n"), "{md}");
    assert!(md.contains("\nverified:\n- by: human:jordi\n"), "{md}");
    assert!(md.ends_with("\nText checked by a person.\n"), "{md}");
    assert_eq!(engine.calls, 0);
}

#[test]
fn a_foreign_okf_file_is_refused() {
    let (_dir, src) = setup();
    let note = src.join("Test 2.note");
    let md = "---\ntype: note\ntitle: Test 2\ngenerated: { by: other/1.0, at: 2026-10-05T23:10:00Z }\n---\n\nMine.\n";
    std::fs::write(src.join("Test 2.md"), md).unwrap();
    let (found, _) = inputs::collect(std::slice::from_ref(&note), None);
    let outputs = inputs::output_paths(&found, None);
    let mut engine = FakeEngine { calls: 0 };
    let err = pipeline::convert_one(
        &found[0],
        &outputs[0],
        &mut engine,
        &opts(None),
        &FileIndex::default(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("not created by notedown"), "{err}");
    assert_eq!(std::fs::read_to_string(&outputs[0]).unwrap(), md);
    assert_eq!(engine.calls, 0);
}

#[test]
fn an_old_file_with_crlf_line_endings_is_read() {
    let (dir, src) = setup();
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let note = src.join("Test 2.note");
    let md_path = out.join("Test 2.md");
    let crlf = old_header_md(&note, "fake-model", OLD_BODY).replace('\n', "\r\n");
    std::fs::write(&md_path, &crlf).unwrap();
    assert!(Frontmatter::parse(&crlf).is_some());
    let mut engine = FakeEngine { calls: 0 };
    assert_eq!(
        convert_note(&note, &out, &mut engine),
        pipeline::Outcome::HeaderUpdated
    );
    assert_eq!(engine.calls, 0);
    let md = std::fs::read_to_string(&md_path).unwrap();
    assert!(md.ends_with(&OLD_BODY.replace('\n', "\r\n")), "{md:?}");
    assert_eq!(Frontmatter::parse(&md).unwrap().status, "draft");
}

#[test]
fn an_old_index_md_is_named_but_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let note = dir.path().join("index.note");
    std::fs::copy(fixtures().join("notes/test2.note"), &note).unwrap();
    let (found, _) = inputs::collect(std::slice::from_ref(&note), None);
    let outputs = inputs::output_paths(&found, None);
    assert_eq!(outputs[0], dir.path().join("index.note.md"));
    let old = dir.path().join("index.md");
    assert_eq!(pipeline::left_over_reserved(&found[0], &outputs[0]), None);
    std::fs::write(
        &old,
        old_header_md(&note, "fake-model", OLD_BODY)
            .replace("source: Test 2.note", "source: index.note"),
    )
    .unwrap();
    assert_eq!(
        pipeline::left_over_reserved(&found[0], &outputs[0]),
        Some(old.clone())
    );
    // another note's file or a file of another tool is not named
    std::fs::write(&old, old_header_md(&note, "fake-model", OLD_BODY)).unwrap();
    assert_eq!(pipeline::left_over_reserved(&found[0], &outputs[0]), None);
    std::fs::write(&old, "# my index\n").unwrap();
    assert_eq!(pipeline::left_over_reserved(&found[0], &outputs[0]), None);
    let mut engine = FakeEngine { calls: 0 };
    pipeline::convert_one(
        &found[0],
        &outputs[0],
        &mut engine,
        &opts(None),
        &FileIndex::default(),
    )
    .unwrap();
    assert_eq!(std::fs::read_to_string(&old).unwrap(), "# my index\n");
}

/// [`crystalline_form`] with the multi-line values that Crystalline writes as block
/// scalars (`|`, `|-`, `|2-`, `|+`).
fn crystalline_multi_line(note: &std::path::Path, at: time::OffsetDateTime) -> String {
    crystalline_form(note, at)
        .replace(
            "keywords:\n- 'p1: it''s'\n",
            "keywords:\n- |-\n  a\n  b\n- |\n  line1\n  line2\n- |-\n  two\n\n  paras\n\
             - |2-\n    indented\n    more\n- |-\n  x\n  ---\n  y\n- |+\n  keep\n\n\n",
        )
        .replace(
            "  text: plain\n",
            "  text: |-\n    first line\n    second line\n",
        )
}

#[test]
fn block_scalars_from_crystalline_read_back_exactly() {
    let (dir, src) = setup();
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let note = src.join("Test 2.note");
    let at = pin_mtime(&note);
    let md_path = out.join("Test 2.md");
    let text = crystalline_multi_line(&note, at);
    std::fs::write(&md_path, &text).unwrap();
    let expected_keywords: Vec<String> = [
        "a\nb",
        "line1\nline2\n",
        "two\n\nparas",
        "  indented\n  more",
        "x\n---\ny",
        "keep\n\n\n",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let fm = Frontmatter::parse(&text).unwrap();
    assert_eq!(fm.keywords, expected_keywords);
    assert_eq!(fm.digests[0].text, "first line\nsecond line");
    let mut engine = FakeEngine { calls: 0 };
    assert_eq!(
        convert_note(&note, &out, &mut engine),
        pipeline::Outcome::Skipped
    );
    // a header-only update writes the same values back in notedown's form
    let moved = dir.path().join("moved");
    std::fs::rename(&src, &moved).unwrap();
    assert_eq!(
        convert_note(&moved.join("Test 2.note"), &out, &mut engine),
        pipeline::Outcome::HeaderUpdated
    );
    let md = std::fs::read_to_string(&md_path).unwrap();
    assert!(md.contains("  - \"  indented\\n  more\"\n"), "{md}");
    let fm = Frontmatter::parse(&md).unwrap();
    assert_eq!(fm.keywords, expected_keywords);
    assert_eq!(fm.digests[0].text, "first line\nsecond line");
    assert_eq!(engine.calls, 0);
}

#[test]
fn a_value_the_reader_does_not_understand_means_a_new_conversion() {
    let (dir, src) = setup();
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let note = src.join("Test 2.note");
    let at = pin_mtime(&note);
    let md_path = out.join("Test 2.md");
    // a tag on a keyword: the header is notedown's, but the keyword cannot be read
    let text = crystalline_form(&note, at).replace("- 'p1: it''s'", "- !!str p1");
    std::fs::write(&md_path, &text).unwrap();
    assert_eq!(Frontmatter::parse(&text), None);
    let mut engine = FakeEngine { calls: 0 };
    assert!(matches!(
        convert_note(&note, &out, &mut engine),
        pipeline::Outcome::Converted { .. }
    ));
    assert_eq!(engine.calls, 1);
    let md = std::fs::read_to_string(&md_path).unwrap();
    assert!(md.contains("\nkeywords: []\n"), "{md}");
    assert!(md.contains("\nverified:\n- by: human:jordi\n"), "{md}");
}

#[test]
fn flow_map_shapes_that_only_look_like_notedown_are_foreign() {
    for header in [
        "generated: { x: a # , by: notedown/1.0.0\n  }\n",
        "generated: { by:notedown/1 }\n",
        "generated: { by: !!str notedown/1 }\n",
    ] {
        let md = format!("---\ntype: note\n{header}---\nbody\n");
        let h = notedown_convert::document::Header::read(&md).expect("readable");
        assert!(!h.is_notedown(), "{header:?}");
    }
}

#[test]
fn a_crlf_header_keeps_its_line_endings_comments_and_added_keys() {
    let (dir, src) = setup();
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let note = src.join("Test 2.note");
    let at = pin_mtime(&note);
    let md_path = out.join("Test 2.md");
    let text = crystalline_form(&note, at)
        .replace("resource: ../in/", "resource: ../old/")
        .replace("verified:\n", "# checked by hand\nverified:\n")
        .replace("type: note\n", "type: note\n# about notedown keys\n")
        .replace('\n', "\r\n");
    std::fs::write(&md_path, &text).unwrap();
    let mut engine = FakeEngine { calls: 0 };
    assert_eq!(
        convert_note(&note, &out, &mut engine),
        pipeline::Outcome::HeaderUpdated
    );
    let md = std::fs::read_to_string(&md_path).unwrap();
    let (head, body) = md.split_at(md.find("\r\n---\r\n").unwrap() + 7);
    assert!(
        !head.replace("\r\n", "").contains('\n'),
        "only CRLF: {head:?}"
    );
    assert!(
        head.ends_with(
            "# about notedown keys\r\n# checked by hand\r\nverified:\r\n- by: human:jordi\r\n  at: 2026-10-06T09:00:00+02:00\r\n---\r\n"
        ),
        "{head:?}"
    );
    assert!(
        head.contains("\r\nresource: \"../in/Test 2.note\"\r\n"),
        "{head:?}"
    );
    assert_eq!(body, text.split_once("\r\n---\r\n").unwrap().1);
    assert_eq!(engine.calls, 0);
}

#[test]
fn an_unknown_value_in_a_flow_generated_map_still_counts_as_notedowns() {
    let (dir, src) = setup();
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let note = src.join("Test 2.note");
    let at = pin_mtime(&note);
    let md_path = out.join("Test 2.md");
    let text =
        crystalline_form(&note, at).replace("model: \"fake-model\"", "model: !!str fake-model");
    assert!(text.contains("model: !!str fake-model, at: "), "{text}");
    std::fs::write(&md_path, &text).unwrap();
    assert!(
        notedown_convert::document::Header::read(&text)
            .unwrap()
            .is_notedown()
    );
    assert_eq!(Frontmatter::parse(&text), None);
    let mut engine = FakeEngine { calls: 0 };
    assert!(matches!(
        convert_note(&note, &out, &mut engine),
        pipeline::Outcome::Converted { .. }
    ));
    assert_eq!(engine.calls, 1);
    let md = std::fs::read_to_string(&md_path).unwrap();
    assert!(md.contains(", model: \"fake-model\", at: "), "{md}");
    assert!(md.contains("\nverified:\n- by: human:jordi\n"), "{md}");
}

#[test]
fn a_kept_keep_chomping_block_scalar_survives_a_rewrite() {
    let (dir, src) = setup();
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let note = src.join("Test 2.note");
    let at = pin_mtime(&note);
    let md_path = out.join("Test 2.md");
    let kept = "note: |+\n  a\n\n\n";
    let text = crystalline_form(&note, at)
        .replace("resource: ../in/", "resource: ../old/")
        .replace("verified:\n", &format!("{kept}verified:\n"));
    std::fs::write(&md_path, &text).unwrap();
    let mut engine = FakeEngine { calls: 0 };
    assert_eq!(
        convert_note(&note, &out, &mut engine),
        pipeline::Outcome::HeaderUpdated
    );
    let md = std::fs::read_to_string(&md_path).unwrap();
    assert!(
        md.contains(&format!("{kept}verified:\n- by: human:jordi\n")),
        "{md}"
    );
    // and again: the value stays "a\n\n\n"
    let moved = dir.path().join("moved");
    std::fs::rename(&src, &moved).unwrap();
    assert_eq!(
        convert_note(&moved.join("Test 2.note"), &out, &mut engine),
        pipeline::Outcome::HeaderUpdated
    );
    let md = std::fs::read_to_string(&md_path).unwrap();
    assert!(md.contains(&format!("{kept}verified:\n")), "{md}");
    assert_eq!(engine.calls, 0);
}
