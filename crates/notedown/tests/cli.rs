use notedown::links::FileIndex;
use notedown::{
    document::{Frontmatter, FrontmatterLink},
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
        Ok(PageOcr { html: r#"<div data-label="Text"><p>Hello page</p></div><div data-label="Diagram"><pre>graph LR
A --> B</pre></div>"#.into(),
            prompt_tokens: 10, output_tokens: 5, truncated: false, encode_ms: 1, decode_ms: 1 })
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
    assert!(md.starts_with("---\nsource: Test 2.note\n"), "{md}");
    assert!(md.contains("file_id: F20261005225535133622IHP5FqtIJoxD"));
    assert!(md.contains("model: fake-model"));
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
        source: "Test 2.note".into(),
        source_sha256: "ab".into(),
        kind: "supernote-note".into(),
        file_id: Some("F1".into()),
        pages: 1,
        model: "m".into(),
        converted_at: "2026-10-05T23:10:00Z".into(),
        generator: "notedown 0.1.0".into(),
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
    };
    let back = Frontmatter::parse(&format!("{}\n## Page 1\n", fm.render())).unwrap();
    assert_eq!(back.source_sha256, "ab");
    assert_eq!(back.keywords, fm.keywords);
    assert_eq!(back.links, fm.links);
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
        md.contains("target_file_id: F20240708174639550429bHksqaP2wKQ8"),
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
            notedown::links::escape_md(stem),
            notedown::links::escape_md(path)
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
    use notedown_ocr::convert::LayoutBlock;
    let l = link(LinkKind::File, [788, 426, 275, 111]);
    // Same area in 0-1000 coordinates of a 1404x1872 page.
    let hit = LayoutBlock {
        label: "Text".into(),
        bbox: Some([561, 227, 757, 287]),
        html: "<p>Bike</p>".into(),
    };
    let miss = LayoutBlock {
        label: "Text".into(),
        bbox: Some([100, 600, 300, 650]),
        html: "<p>Alpha</p>".into(),
    };
    assert_eq!(
        notedown::links::place(&[l], &[miss, hit], 1404, 1872),
        vec![Some(1)]
    );
}

/// Always-running coverage of resolution: a FileIndex from a fake `.md` frontmatter
/// and links applied to synthetic OCR pages.
#[test]
fn apply_links_resolves_by_file_id_without_private_files() {
    use notedown::document::PageOut;
    use notedown::pipeline::PageResult;
    use notedown_note::LinkKind;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir_all(out.join("sub")).unwrap();
    let fm = Frontmatter {
        source: "Bike.note".into(),
        source_sha256: "ab".into(),
        kind: "supernote-note".into(),
        file_id: Some("FBIKE".into()),
        pages: 1,
        model: "m".into(),
        converted_at: "2026-10-05T23:10:00Z".into(),
        generator: "notedown 0.1.0".into(),
        keywords: vec![],
        links: vec![],
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
        notedown_ocr::convert::LayoutBlock {
            label: "Text".into(),
            bbox: Some([561, 227, 757, 287]),
            html: "<p><u>Bike @</u></p>".into(),
        },
        notedown_ocr::convert::LayoutBlock {
            label: "Text".into(),
            bbox: Some([570, 700, 770, 760]),
            html: "<p><u>Bata @</u></p>".into(),
        },
        notedown_ocr::convert::LayoutBlock {
            label: "Text".into(),
            bbox: Some([100, 600, 300, 650]),
            html: "<p>Alpha</p>".into(),
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
    let ctx = notedown::links::LinkContext {
        index: &index,
        from_md: &out.join("Content.md"),
        page_ids: &[None],
        width: 1404,
        height: 1872,
    };
    let fm_links = notedown::links::apply(&[l1, l2], &mut pages, &ctx);
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

fn block(bbox: [u32; 4], text: &str) -> notedown_ocr::convert::LayoutBlock {
    notedown_ocr::convert::LayoutBlock {
        label: "Text".into(),
        bbox: Some(bbox),
        html: format!("<p>{text}</p>"),
    }
}

#[test]
fn link_replaces_a_block_only_when_it_covers_half_of_it() {
    use notedown::links::place;
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
    use notedown::document::PageOut;
    use notedown::pipeline::PageResult;
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
    let ctx = notedown::links::LinkContext {
        index: &index,
        from_md: std::path::Path::new("Alpha.md"),
        page_ids: &[None],
        width: 1000,
        height: 1000,
    };
    notedown::links::apply(&[l], &mut pages, &ctx);
    let md = &pages[0].out.markdown;
    assert!(md.starts_with(text), "{md}");
    assert!(
        md.ends_with("Links: Beta (/storage/emulated/0/Note/Beta.note)\n"),
        "{md}"
    );
}

#[test]
fn note_strings_in_the_body_are_escaped() {
    use notedown::document::PageOut;
    use notedown::pipeline::PageResult;
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
    let ctx = notedown::links::LinkContext {
        index: &index,
        from_md: std::path::Path::new("Alpha.md"),
        page_ids: &[None],
        width: 1000,
        height: 1000,
    };
    notedown::links::apply(&[l], &mut pages, &ctx);
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
    use notedown::document::{PageOut, render_document};
    let fm = Frontmatter {
        source: "A.note".into(),
        source_sha256: "ab".into(),
        kind: "supernote-note".into(),
        file_id: None,
        pages: 1,
        model: "m".into(),
        converted_at: "2026-10-05T23:10:00Z".into(),
        generator: "notedown 0.1.0".into(),
        keywords: vec![],
        links: vec![],
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
