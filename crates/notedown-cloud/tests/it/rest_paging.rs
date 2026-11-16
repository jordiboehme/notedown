//! Paged `GET /tree` and `GET /recycle`: the order of the pages, their cursors and what a
//! continuation page costs.

use std::collections::HashSet;
use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use notedown_cloud::store::WirePath;
use serde_json::{Value, json};

use crate::rest_server::{Server, delete, q, start_imported_with, tree_all, uploaded, writer};

/// An imported account on a server without the watch and the timer scan, so no scan runs
/// between the requests of a test: what a page costs and what it journals is its own.
fn start_imported() -> Server {
    start_imported_with(|c| {
        c.watch = false;
        c.scan_interval = 0;
    })
}

fn files(s: &Server) -> PathBuf {
    s.state.store("jordi").unwrap().files_dir().to_path_buf()
}

/// Writes a file by hand below `files/`.
fn hand(s: &Server, rel: &str, bytes: &[u8]) {
    let p = files(s).join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

/// Makes a folder by hand below `files/`.
fn hand_dir(s: &Server, rel: &str) {
    std::fs::create_dir_all(files(s).join(rel)).unwrap();
}

/// `n` files made by hand in `dir`; their wire paths in the order of the listing.
fn many(s: &Server, dir: &str, n: usize) -> Vec<String> {
    let base = files(s).join(dir);
    std::fs::create_dir_all(&base).unwrap();
    (0..n)
        .map(|i| {
            let name = format!("f{i:05}.txt");
            std::fs::write(base.join(&name), i.to_string()).unwrap();
            format!("{dir}/{name}")
        })
        .collect()
}

/// A cursor as the server builds it: `prefix` and the anchor in base64url.
fn cursor(prefix: &str, anchor: &str) -> String {
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(anchor))
}

/// `GET` of `url` (200 expected).
fn page(s: &Server, token: &str, url: &str) -> Value {
    let (status, answer) = s.get(Some(token), url);
    assert_eq!(status, 200, "{url}: {answer}");
    answer
}

fn paths(entries: &[Value]) -> Vec<String> {
    entries
        .iter()
        .map(|e| e["path"].as_str().unwrap().to_string())
        .collect()
}

fn page_paths(page: &Value) -> Vec<String> {
    paths(page["entries"].as_array().unwrap())
}

/// Every page of `/tree?<query>` along `next`, as the paths of each page.
fn pages(s: &Server, token: &str, query: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut url = format!("/tree?{query}");
    loop {
        let p = page(s, token, &url);
        out.push(page_paths(&p));
        match p["next"].as_str() {
            Some(next) => url = format!("/tree?{query}&cursor={next}"),
            None => {
                assert_eq!(p["next"], Value::Null, "{p}");
                return out;
            }
        }
    }
}

/// `GET` of `url` answers 400 `bad_cursor`.
fn refused(s: &Server, token: &str, url: &str) {
    let (status, answer) = s.get(Some(token), url);
    assert_eq!(
        (status, &answer["error"]),
        (400, &json!("bad_cursor")),
        "{url}: {answer}"
    );
}

#[test]
fn tree_pages_through_a_large_folder() {
    let s = start_imported();
    let t = s.token();
    let want = many(&s, "NOTE/Note/big", 20_000);
    let got = pages(&s, &t, &format!("path={}&limit=1000", q("NOTE/Note/big")));
    assert_eq!(got.len(), 20);
    assert!(got.iter().all(|p| p.len() == 1000));
    let all = got.concat();
    let unique: HashSet<&String> = all.iter().collect();
    assert_eq!(unique.len(), all.len(), "an entry came twice");
    assert_eq!(all, want);
}

#[test]
fn continuation_pages_do_not_scan_the_folder() {
    let s = start_imported();
    let t = s.token();
    many(&s, "NOTE/Note/big", 20_000);
    let store = s.state.store("jordi").unwrap();
    let base = format!("/tree?path={}&limit=1000", q("NOTE/Note/big"));

    let (scans, stats) = store.walk_counts();
    let first = page(&s, &t, &base);
    let (scans_1, stats_1) = store.walk_counts();
    // The first page scans once and lists from that walk.
    assert_eq!(scans_1 - scans, 1);
    assert!(
        (20_000..30_000).contains(&(stats_1 - stats)),
        "{}",
        stats_1 - stats
    );

    let mut next = first["next"].as_str().unwrap().to_string();
    for _ in 0..3 {
        let (scans, stats) = store.walk_counts();
        let p = page(&s, &t, &format!("{base}&cursor={next}"));
        let (scans_1, stats_1) = store.walk_counts();
        assert_eq!(scans_1, scans, "a continuation page scanned");
        assert!(
            (1000..=1010).contains(&(stats_1 - stats)),
            "a continuation page stated {} entries",
            stats_1 - stats
        );
        assert_eq!(p["entries"].as_array().unwrap().len(), 1000);
        next = p["next"].as_str().unwrap().to_string();
    }
}

#[test]
fn tree_pages_keep_the_segment_order() {
    let s = start_imported();
    let t = s.token();
    let dir = "NOTE/Note/order";
    hand(&s, &format!("{dir}/a/x.txt"), b"x");
    hand(&s, &format!("{dir}/a b/y.txt"), b"y");
    for name in ["a-b", "a.b", "a(1)", "a+b", "a&b", "a'b", "ä"] {
        hand(&s, &format!("{dir}/{name}"), name.as_bytes());
    }
    // Each name compared by its bytes: ' ' < '&' < '\'' < '(' < '+' < '-' < '.' < 'ä'.
    let flat = ["a", "a b", "a&b", "a'b", "a(1)", "a+b", "a-b", "a.b", "ä"];
    let deep = [
        "a",
        "a/x.txt",
        "a b",
        "a b/y.txt",
        "a&b",
        "a'b",
        "a(1)",
        "a+b",
        "a-b",
        "a.b",
        "ä",
    ];
    let full =
        |names: &[&str]| -> Vec<String> { names.iter().map(|n| format!("{dir}/{n}")).collect() };
    for limit in [1, 2, 3, 1000] {
        let got = pages(&s, &t, &format!("path={}&limit={limit}", q(dir)));
        assert_eq!(got.len(), flat.len().div_ceil(limit), "limit {limit}");
        assert_eq!(got.concat(), full(&flat), "limit {limit}");
        let got = pages(
            &s,
            &t,
            &format!("path={}&recursive=true&limit={limit}", q(dir)),
        );
        assert_eq!(got.len(), deep.len().div_ceil(limit), "limit {limit}");
        assert_eq!(got.concat(), full(&deep), "recursive, limit {limit}");
    }
}

#[test]
fn tree_pages_a_recursive_walk() {
    let s = start_imported();
    let t = s.token();
    let dir = "NOTE/Note/deep";
    for rel in [
        "a/1.txt",
        "a/b/2.txt",
        "a/b/c/3.txt",
        "a/b/c/4.txt",
        "a/z.txt",
        "b/5.txt",
        "c.txt",
        "d/e/f/6.txt",
    ] {
        hand(&s, &format!("{dir}/{rel}"), rel.as_bytes());
    }
    hand_dir(&s, &format!("{dir}/d/empty"));
    let one = page(
        &s,
        &t,
        &format!("/tree?path={}&recursive=true&limit=1000", q(dir)),
    );
    assert_eq!(one["next"], Value::Null);
    let want = page_paths(&one);
    assert_eq!(want.len(), 16, "{want:?}");
    let mut sorted: Vec<WirePath> = want.iter().map(|p| WirePath::parse(p).unwrap()).collect();
    sorted.sort();
    let sorted: Vec<String> = sorted.iter().map(WirePath::display).collect();
    assert_eq!(want, sorted);
    for limit in 1..=5 {
        let got = pages(
            &s,
            &t,
            &format!("path={}&recursive=true&limit={limit}", q(dir)),
        );
        assert!(got.iter().all(|p| p.len() <= limit));
        assert_eq!(got.concat(), want, "limit {limit}");
    }
    // The root too, page by page, is the one long listing.
    let all = pages(&s, &t, "recursive=true&limit=1000").concat();
    assert_eq!(pages(&s, &t, "recursive=true&limit=3").concat(), all);
}

#[test]
fn a_tree_cursor_after_a_deleted_anchor_continues() {
    let s = start_imported();
    let t = s.token();
    let dir = "NOTE/Note/gone";
    for i in 1..=6 {
        hand(&s, &format!("{dir}/f{i}"), b"f");
    }
    let base = format!("/tree?path={}&limit=2", q(dir));
    let first = page(&s, &t, &base);
    assert_eq!(
        page_paths(&first),
        [format!("{dir}/f1"), format!("{dir}/f2")]
    );
    std::fs::remove_file(files(&s).join(dir).join("f2")).unwrap();
    std::fs::remove_file(files(&s).join(dir).join("f3")).unwrap();
    let next = first["next"].as_str().unwrap();
    let second = page(&s, &t, &format!("{base}&cursor={next}"));
    assert_eq!(
        page_paths(&second),
        [format!("{dir}/f4"), format!("{dir}/f5")]
    );

    // Recursive: the folder on the anchor's way is gone with all in it.
    let dir = "NOTE/Note/gone2";
    for rel in ["a/1", "a/2", "b/1", "c"] {
        hand(&s, &format!("{dir}/{rel}"), b"x");
    }
    let base = format!("/tree?path={}&recursive=true&limit=2", q(dir));
    let first = page(&s, &t, &base);
    assert_eq!(
        page_paths(&first),
        [format!("{dir}/a"), format!("{dir}/a/1")]
    );
    std::fs::remove_dir_all(files(&s).join(dir).join("a")).unwrap();
    let next = first["next"].as_str().unwrap();
    let second = page(&s, &t, &format!("{base}&cursor={next}"));
    assert_eq!(
        page_paths(&second),
        [format!("{dir}/b"), format!("{dir}/b/1")]
    );
    let next = second["next"].as_str().unwrap();
    let third = page(&s, &t, &format!("{base}&cursor={next}"));
    assert_eq!(page_paths(&third), [format!("{dir}/c")]);
    assert_eq!(third["next"], Value::Null);
}

#[test]
fn a_cursor_outside_the_scope_is_bad_cursor() {
    let s = start_imported();
    let t = writer(&s);
    for rel in ["A/1", "A/2", "A/sub/x", "B/1", "B/2"] {
        hand(&s, &format!("NOTE/Note/{rel}"), b"x");
    }
    let a = q("NOTE/Note/A");
    let b = q("NOTE/Note/B");
    let first = page(&s, &t, &format!("/tree?path={a}&limit=1"));
    let of_a = first["next"].as_str().unwrap();
    // Another folder.
    refused(&s, &t, &format!("/tree?path={b}&cursor={of_a}"));
    // Not a direct child without recursive; fine with it.
    let deep = cursor("t1:", "NOTE/Note/A/sub/x");
    refused(&s, &t, &format!("/tree?path={a}&cursor={deep}"));
    page(
        &s,
        &t,
        &format!("/tree?path={a}&recursive=true&cursor={deep}"),
    );
    // Hidden, the scope itself, a path that does not parse.
    for anchor in [
        "NOTE/Note/A/.h",
        "NOTE/Note/A/.h/x",
        "NOTE/Note/A",
        "NOTE/Note",
        "",
        "NOTE/Note/A/../B/1",
    ] {
        let c = cursor("t1:", anchor);
        refused(&s, &t, &format!("/tree?path={a}&recursive=true&cursor={c}"));
    }
    // Garbled.
    let not_utf8 = format!("t1:{}", URL_SAFE_NO_PAD.encode([0xff, 0xfe]));
    let other_version = cursor("t2:", "NOTE/Note/A/1");
    for c in ["nonsense", "t1:", "t1:!!", "t1", &not_utf8, &other_version] {
        refused(&s, &t, &format!("/tree?path={a}&cursor={c}"));
    }
    // The cursor of another route.
    for name in ["x", "y"] {
        let id = uploaded(&s, &t, &format!("NOTE/Note/{name}.txt"), name.as_bytes());
        assert_eq!(delete(&s, &t, id).0, 200);
    }
    let bin = page(&s, &t, "/recycle?limit=1");
    let recycle = bin["next"].as_str().unwrap();
    refused(&s, &t, &format!("/tree?path={a}&cursor={recycle}"));
    refused(&s, &t, &format!("/recycle?cursor={of_a}"));
}

#[test]
fn kind_folder_lists_only_folders() {
    let s = start_imported();
    let t = s.token();
    let dir = "NOTE/Note/mixed";
    for i in 0..5 {
        hand(&s, &format!("{dir}/a{i}.txt"), b"f");
        hand(&s, &format!("{dir}/b{i}/inner.txt"), b"f");
        hand_dir(&s, &format!("{dir}/b{i}/sub"));
    }
    let want: Vec<String> = (0..5).map(|i| format!("{dir}/b{i}")).collect();
    for limit in [1, 2, 1000] {
        let got = pages(
            &s,
            &t,
            &format!("path={}&kind=folder&limit={limit}", q(dir)),
        );
        assert!(got.iter().all(|p| p.len() <= limit));
        assert_eq!(got.concat(), want, "limit {limit}");
    }
    let deep: Vec<String> = (0..5)
        .flat_map(|i| [format!("{dir}/b{i}"), format!("{dir}/b{i}/sub")])
        .collect();
    for limit in [1, 3, 1000] {
        let got = pages(
            &s,
            &t,
            &format!("path={}&recursive=true&kind=folder&limit={limit}", q(dir)),
        );
        assert_eq!(got.concat(), deep, "recursive, limit {limit}");
    }
    let (status, answer) = s.get(Some(&t), &format!("/tree?path={}&kind=file", q(dir)));
    assert_eq!((status, &answer["error"]), (400, &json!("bad_request")));
}

#[test]
fn tree_limit_is_clamped() {
    let s = start_imported();
    let t = s.token();
    let dir = q("NOTE/Note/clamp");
    many(&s, "NOTE/Note/clamp", 1_200);
    let count = |url: &str| {
        let p = page(&s, &t, url);
        assert!(p["next"].is_string(), "{url}");
        (p["entries"].as_array().unwrap().len(), p)
    };
    assert_eq!(count(&format!("/tree?path={dir}")).0, 500);
    assert_eq!(count(&format!("/tree?path={dir}&limit=5000")).0, 1000);
    let (n, first) = count(&format!("/tree?path={dir}&limit=0"));
    assert_eq!(n, 1);
    let next = first["next"].as_str().unwrap();
    assert_eq!(
        count(&format!("/tree?path={dir}&limit=0&cursor={next}")).0,
        1
    );
    assert_eq!(
        count(&format!("/tree?path={dir}&limit=5000&cursor={next}")).0,
        1000
    );
    let (status, answer) = s.get(Some(&t), &format!("/tree?path={dir}&limit=x"));
    assert_eq!((status, &answer["error"]), (400, &json!("bad_request")));
}

#[test]
fn a_paged_root_walk_keeps_the_hash_cache() {
    let s = start_imported();
    let t = s.token();
    let mut all = Vec::new();
    for dir in ["NOTE/Note/h1", "NOTE/Note/h2", "DOCUMENT/Document/h3"] {
        all.extend(many(&s, dir, 10));
    }
    let first = page(&s, &t, "/tree?recursive=true&limit=5");
    assert!(first["next"].is_string(), "{first}");
    let (_, entries) = tree_all(&s, &t, "recursive=true&limit=5");
    let listed: HashSet<String> = paths(&entries).into_iter().collect();
    let store = s.state.store("jordi").unwrap();
    let g = store.lock().unwrap();
    for p in &all {
        assert!(listed.contains(p), "{p}");
        let md5 = store.cached_md5(&g, &WirePath::parse(p).unwrap()).unwrap();
        assert!(md5.is_some(), "{p} lost its cache row");
    }
    drop(g);
    // The cache file on disk holds them too.
    let text = std::fs::read_to_string(s.data.join("accounts/jordi/hashes.json")).unwrap();
    for p in &all {
        assert!(text.contains(p.as_str()), "{p} is not in hashes.json");
    }
}

#[test]
fn a_recycle_cursor_pages() {
    let s = start_imported();
    let t = writer(&s);
    for name in ["a", "b", "c"] {
        let id = uploaded(&s, &t, &format!("NOTE/Note/{name}.txt"), name.as_bytes());
        assert_eq!(delete(&s, &t, id).0, 200);
    }
    let all = page(&s, &t, "/recycle");
    assert_eq!(all["next"], Value::Null);
    assert!(all.get("more").is_none(), "{all}");
    let keys: Vec<String> = all["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["key"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(keys.len(), 3);
    let mut got = Vec::new();
    let mut url = "/recycle?limit=1".to_string();
    loop {
        let p = page(&s, &t, &url);
        got.extend(
            p["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["key"].as_str().unwrap().to_string()),
        );
        match p["next"].as_str() {
            Some(next) => url = format!("/recycle?limit=1&cursor={next}"),
            None => break,
        }
    }
    assert_eq!(got, keys);
    // A key that left the bin still has its place in the order.
    let (ms, path) = keys[0].split_once('/').unwrap();
    let gone = cursor("r1:", &format!("{ms}/{path}x"));
    let p = page(&s, &t, &format!("/recycle?cursor={gone}"));
    let rest: Vec<&str> = p["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["key"].as_str().unwrap())
        .collect();
    assert_eq!(
        rest,
        keys[1..].iter().map(String::as_str).collect::<Vec<_>>()
    );
    for bad in [
        cursor("r1:", "nokey"),
        cursor("r1:", "x/NOTE"),
        "r1:!!".into(),
    ] {
        refused(&s, &t, &format!("/recycle?cursor={bad}"));
    }
}

#[test]
fn a_hand_change_on_a_continuation_page_is_journaled() {
    let s = start_imported();
    let t = s.token();
    let dir = "NOTE/Note/late";
    for i in 1..=4 {
        hand(&s, &format!("{dir}/f{i}"), b"old");
    }
    let base = format!("/tree?path={}&limit=2", q(dir));
    let first = page(&s, &t, &base);
    let seq = first["seq"].as_u64().unwrap();
    // After the first page: a new file and a changed one, both after the anchor.
    hand(&s, &format!("{dir}/f3b"), b"new");
    hand(&s, &format!("{dir}/f4"), b"changed by hand");
    let next = first["next"].as_str().unwrap();
    let second = page(
        &s,
        &t,
        &format!("/tree?path={}&limit=10&cursor={next}", q(dir)),
    );
    let entries = second["entries"].as_array().unwrap();
    assert_eq!(
        paths(entries),
        [
            format!("{dir}/f3"),
            format!("{dir}/f3b"),
            format!("{dir}/f4")
        ]
    );
    let f4 = &entries[2];
    assert_eq!(f4["size"], 15);
    let events = page(&s, &t, &format!("/events?from={}", seq + 1));
    let events = events["events"].as_array().unwrap();
    let of = |path: String| -> Vec<&str> {
        events
            .iter()
            .filter(|e| e["path"] == path)
            .map(|e| e["type"].as_str().unwrap())
            .collect()
    };
    assert_eq!(of(format!("{dir}/f3b")), ["file.created"], "{events:?}");
    assert_eq!(of(format!("{dir}/f4")), ["file.modified"], "{events:?}");
    let created = events
        .iter()
        .find(|e| e["path"] == format!("{dir}/f3b"))
        .unwrap();
    assert_eq!(created["id"], entries[1]["id"]);
}

#[test]
fn a_hand_rename_on_a_continuation_page_is_a_move() {
    let s = start_imported();
    let t = s.token();
    let dir = "NOTE/Note/renamed";
    for i in 1..=4 {
        hand(
            &s,
            &format!("{dir}/f{i}"),
            format!("content {i}").as_bytes(),
        );
    }
    let base = format!("/tree?path={}&limit=2", q(dir));
    let first = page(&s, &t, &base);
    let seq = first["seq"].as_u64().unwrap();
    let old_id = s
        .state
        .store("jordi")
        .unwrap()
        .id_of(&WirePath::parse(&format!("{dir}/f3")).unwrap())
        .unwrap()
        .unwrap();
    let d = files(&s).join(dir);
    std::fs::rename(d.join("f3"), d.join("f5")).unwrap();
    let next = first["next"].as_str().unwrap();
    let second = page(&s, &t, &format!("{base}&cursor={next}"));
    let entries = second["entries"].as_array().unwrap();
    assert_eq!(paths(entries), [format!("{dir}/f4"), format!("{dir}/f5")]);
    assert_eq!(entries[1]["id"], old_id);
    let events = page(&s, &t, &format!("/events?from={}", seq + 1));
    let events = events["events"].as_array().unwrap();
    let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["file.moved"], "{events:?}");
    assert_eq!(events[0]["id"], old_id);
    assert_eq!(events[0]["path"], format!("{dir}/f5"));
    assert_eq!(events[0]["oldPath"], format!("{dir}/f3"));
}

#[test]
fn a_continuation_page_of_a_gone_or_replaced_folder_is_404_or_409() {
    let s = start_imported();
    let t = s.token();
    for dir in ["NOTE/Note/gone", "NOTE/Note/replaced"] {
        for i in 1..=3 {
            hand(&s, &format!("{dir}/f{i}"), b"x");
        }
    }
    let next_of = |dir: &str| {
        let p = page(&s, &t, &format!("/tree?path={}&limit=1", q(dir)));
        p["next"].as_str().unwrap().to_string()
    };
    let gone = next_of("NOTE/Note/gone");
    let replaced = next_of("NOTE/Note/replaced");
    let root = files(&s);
    std::fs::remove_dir_all(root.join("NOTE/Note/gone")).unwrap();
    std::fs::remove_dir_all(root.join("NOTE/Note/replaced")).unwrap();
    std::fs::write(root.join("NOTE/Note/replaced"), b"a file now").unwrap();
    let (status, answer) = s.get(
        Some(&t),
        &format!("/tree?path={}&limit=1&cursor={gone}", q("NOTE/Note/gone")),
    );
    assert_eq!((status, answer), (404, json!({"error": "not_found"})));
    let (status, answer) = s.get(
        Some(&t),
        &format!(
            "/tree?path={}&limit=1&cursor={replaced}",
            q("NOTE/Note/replaced")
        ),
    );
    assert_eq!((status, answer), (409, json!({"error": "not_a_folder"})));
}

#[test]
fn the_old_after_of_recycle_is_refused() {
    let s = start_imported();
    let t = s.token();
    let (status, answer) = s.get(Some(&t), "/recycle?after=1%2FNOTE%2Fx");
    assert_eq!((status, &answer["error"]), (400, &json!("bad_request")));
    assert!(
        answer["message"].as_str().unwrap().contains("cursor"),
        "{answer}"
    );
}
