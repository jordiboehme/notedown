//! The review against a routed server on loopback, a fake sidecar that writes PNG files and a
//! temp data folder and cache. Nothing real sleeps to decide a result: a test waits for a
//! condition (a file, a count, a process) with a bound.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use md5::Md5;
use serde_json::{Value, json};

use super::*;
use crate::config::{AppConfig, Mode};
use crate::library::LibraryState;
use crate::process::spawn_program;
use crate::rest::test_http::{Act, Routed, answer, file_answer};
use crate::serve_info::ServeInfo;
use crate::test_procs::{gone, kill_in, left_in, tagged_sleep};
use crate::token::{MemoryStore, Token, TokenStore, token_key};

const ACCOUNT: &str = "jordi";
const NOTE: &str = "NOTE/Note/Plan.note";
const BYTES: &[u8] = b"the bytes of the note";
/// How long a test waits for a condition before it fails.
const WAIT: Duration = Duration::from_secs(10);

fn md5_hex(bytes: &[u8]) -> String {
    crate::login::hex(&Md5::digest(bytes))
}

/// `"GET /rest/v1/entries?path=<path>"` as the client encodes it.
fn look(path: &str) -> String {
    let mut url = reqwest::Url::parse("http://h/rest/v1/entries").unwrap();
    url.query_pairs_mut().append_pair("path", path);
    format!("GET {}?{}", url.path(), url.query().unwrap())
}

const FILE: &str = "GET /rest/v1/files/41";

fn entry_of(md5: Value, kind: &str) -> Act {
    Act::Answer(answer(
        "200 OK",
        &json!({"id": 41, "path": NOTE, "name": "Plan.note", "kind": kind,
                "size": BYTES.len(), "md5": md5, "mtimeMs": 1, "writable": true})
        .to_string(),
    ))
}

fn entry() -> Act {
    entry_of(json!(md5_hex(BYTES)), "file")
}

fn bytes_of(body: &[u8]) -> Act {
    Act::Raw(file_answer(
        "200 OK",
        &[("etag", &format!("\"{}\"", md5_hex(body)))],
        body,
    ))
}

/// The fake `notedown`: every call goes to `calls`. `render <file> ... --page N --out D`
/// writes a PNG `D/Plan-page-00N.png` that holds the page and the note's bytes, and prints its
/// path. With `fail` it reports the page as failed; with `hold` it waits for `release` or
/// `release-<N>` (10 s at
/// most) before it writes; with `linger` it writes, prints and then runs this test's own sleep
/// of the length in `linger`.
fn sidecar(dir: &Path) -> SpawnFn {
    let d = dir.display();
    let s = tagged_sleep(dir);
    let s = s.display();
    let script = format!(
        r#"#!/bin/sh
S="{s}"
echo "$*" >> "{d}/calls"
[ "$1" = render ] || exit 2
file="$2"
page=""
out=""
shift 2
while [ $# -gt 0 ]; do
  case "$1" in
  --page) page="$2"; shift 2 ;;
  --out) out="$2"; shift 2 ;;
  *) shift ;;
  esac
done
if [ -f "{d}/fail" ]; then
  echo "error: page $page: the note has 1 page and pages count from 1" >&2
  exit 1
fi
if [ -f "{d}/hold" ]; then
  i=0
  while [ ! -f "{d}/release" ] && [ ! -f "{d}/release-$page" ] && [ "$i" -lt 1000 ]; do
    sleep 0.01; i=$((i + 1))
  done
fi
png="$out/Plan-page-$(printf %03d "$page").png"
printf '\211PNG\r\n\032\npage %s of %s' "$page" "$(cat "$file")" > "$png"
echo "$png"
if [ -f "{d}/linger" ]; then exec "$S" "$(cat "{d}/linger")"; fi
"#
    );
    let path = dir.join("notedown");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    spawn_program(path)
}

/// The PNG the fake writes for `page` (1-based) of a note with `bytes`.
fn png_of(page: usize, bytes: &[u8]) -> Vec<u8> {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(format!("page {page} of ").as_bytes());
    png.extend_from_slice(bytes);
    png
}

fn data_url(png: &[u8]) -> String {
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png)
    )
}

struct Setup {
    server: Routed,
    review: Review,
    /// The fake sidecar, its calls and its sleep.
    bin: tempfile::TempDir,
    /// The data folder with the pipeline's store.
    data: tempfile::TempDir,
    _config: tempfile::TempDir,
    /// The app cache (`<root>/cache`).
    root: tempfile::TempDir,
}

impl Drop for Setup {
    fn drop(&mut self) {
        self.review.quit();
        self.review.join();
        kill_in(self.bin.path());
    }
}

fn setup(routes: Vec<(String, Act)>) -> Setup {
    setup_with_limit(routes, CACHE_LIMIT)
}

fn setup_with_limit(routes: Vec<(String, Act)>, limit: u64) -> Setup {
    let server = Routed::start_with(routes, |_| {});
    let bin = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let config = AppConfig {
        mode: Mode::Remote,
        server_url: Some(format!("http://127.0.0.1:{}", server.port)),
        account: Some(ACCOUNT.to_owned()),
        data_dir: data.path().to_path_buf(),
        ..AppConfig::default()
    };
    let store = Arc::new(MemoryStore::new());
    store
        .set(&token_key(&config).unwrap(), &Token::new("secret"))
        .unwrap();
    let time = Arc::new(AtomicU64::new(0));
    let clock: Clock = {
        let time = time.clone();
        Arc::new(move || time.load(Ordering::SeqCst))
    };
    let place = config.clone();
    let library = Library::with_timing(
        store,
        Arc::new(move || (place.clone(), ServeInfo::default())),
        Arc::new(|_| {}),
        clock.clone(),
        Arc::new(|_| {}),
    );
    library.refresh();
    assert_eq!(library.state().unwrap().state, LibraryState::Ready);
    let data_dir = data.path().to_path_buf();
    let notes = Notes::new(
        Arc::new(move || (data_dir.clone(), Some(ACCOUNT.to_owned()))),
        config_dir.path().to_path_buf(),
        Arc::new(|| {}),
    );
    fs::create_dir_all(store_dir(data.path())).unwrap();
    let review = Review::new(
        library,
        notes,
        sidecar(bin.path()),
        root.path().join("cache"),
        clock,
        limit,
    );
    let s = Setup {
        server,
        review,
        bin,
        data,
        _config: config_dir,
        root,
    };
    // The store has both notes with 12 pages, so page indexes up to 11 are in the note.
    s.store(&stored(&md5_hex(BYTES), pages(12)));
    s.store(&stored_at("F_other", OTHER, pages(12)));
    s
}

const OTHER: &str = "NOTE/Note/Other.note";

/// `n` pages without text.
fn pages(n: usize) -> Vec<Value> {
    (0..n)
        .map(|i| page(&format!("P{i}"), i, "done", vec![]))
        .collect()
}

fn store_dir(data: &Path) -> PathBuf {
    data.join("pipeline").join(ACCOUNT).join("notes")
}

impl Setup {
    fn cache(&self) -> PathBuf {
        self.root.path().join("cache")
    }

    /// Writes the store file of a note as the pipeline does.
    fn store(&self, note: &Value) {
        let key = note["key"].as_str().unwrap();
        let dir = store_dir(self.data.path());
        let temp = dir.join(format!(".{key}.json.1.1.tmp"));
        fs::write(&temp, note.to_string()).unwrap();
        fs::rename(&temp, dir.join(format!("{key}.json"))).unwrap();
    }

    /// The calls of the fake sidecar, one line each.
    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.bin.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn flag(&self, name: &str, value: &str) {
        fs::write(self.bin.path().join(name), value).unwrap();
    }

    /// Every file under the cache, relative to it, sorted.
    fn cached(&self) -> Vec<String> {
        let mut found = Vec::new();
        walk(&self.cache(), &self.cache(), &mut found);
        found.sort();
        found
    }

    fn wait_until(&self, what: &str, done: impl Fn() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(
                start.elapsed() < WAIT,
                "waited in vain for {what}; still running: {}",
                left_in(self.bin.path())
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn walk(root: &Path, dir: &Path, found: &mut Vec<String>) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            found.push(format!(
                "{}/",
                path.strip_prefix(root).unwrap().to_string_lossy()
            ));
            walk(root, &path, found);
        } else {
            found.push(path.strip_prefix(root).unwrap().to_string_lossy().into());
        }
    }
}

fn block(id: &str, text: &str) -> Value {
    json!({"id": id, "label": "text", "bbox": [0, 0, 10, 10], "text": text})
}

/// A store file with the real field names (strokes and the device's recognition included).
fn stored(md5: &str, pages: Vec<Value>) -> Value {
    json!({
        "key": "F20241104", "path": NOTE, "title": "Plan", "size": BYTES.len(), "md5": md5,
        "meta": {"keywords": [], "titles": [], "links": [], "stars": [],
                 "recognition": {"P1": "device text"}, "digests": []},
        "pages": pages, "deleted": false, "successors": {}, "nextBlock": 4, "logSeq": 9,
    })
}

/// A store file of another note.
fn stored_at(key: &str, path: &str, pages: Vec<Value>) -> Value {
    let mut note = stored(&md5_hex(BYTES), pages);
    note["key"] = json!(key);
    note["path"] = json!(path);
    note["title"] = json!("");
    note
}

fn page(id: &str, position: usize, state: &str, blocks: Vec<Value>) -> Value {
    json!({"pageId": id, "position": position, "pageHash": "h", "strokesHash": "s",
           "layersHash": "l", "seq": 3, "state": state,
           "strokeList": [{"id": 1}], "blocks": blocks})
}

#[test]
fn note_open_gives_the_markdown_per_page() {
    let s = setup(vec![(look(NOTE), entry()), (look(OTHER), entry())]);
    let mut landscape = page("P2", 1, "done", vec![block("b3", "| a | b |")]);
    landscape["landscape"] = json!(true);
    s.store(&stored(
        &md5_hex(BYTES),
        vec![
            page(
                "P1",
                0,
                "done",
                vec![
                    block("b1", "# Plan\n"),
                    block("b2", "first line\nsecond  \n"),
                ],
            ),
            landscape,
            page("P3", 2, "failed", vec![]),
            page(
                "P4",
                3,
                "truncated",
                vec![block("b4", "cut <script>x</script>")],
            ),
        ],
    ));
    let view = s.review.open(NOTE).unwrap();
    assert_eq!(view.key, "F20241104");
    assert_eq!(view.path, NOTE);
    assert_eq!(view.title, "Plan");
    assert!(!view.catching_up, "the store has the md5 of the entry");
    let pages: Vec<(usize, PageState, &str, bool)> = view
        .pages
        .iter()
        .map(|p| (p.index, p.state, p.markdown.as_str(), p.landscape))
        .collect();
    assert_eq!(
        pages,
        vec![
            (0, PageState::Done, "# Plan\n\nfirst line\nsecond", false),
            (1, PageState::Done, "| a | b |", true),
            (2, PageState::Failed, "", false),
            (3, PageState::Truncated, "cut <script>x</script>", false),
        ]
    );
    // The answer is camelCase, as the window reads it.
    let wire = serde_json::to_value(&view).unwrap();
    assert_eq!(wire["catchingUp"], json!(false));
    assert_eq!(wire["pages"][1]["landscape"], json!(true));
    assert_eq!(wire["pages"][2]["state"], json!("failed"));
    // The title of a note without one is its file name.
    assert_eq!(s.review.open(OTHER).unwrap().title, "Other");
    // Reading the note renders nothing and downloads nothing.
    assert!(s.calls().is_empty());
    assert!(
        s.server
            .received()
            .iter()
            .all(|r| !r.target.contains("/files/"))
    );
}

#[test]
fn a_note_with_pending_pages_says_so() {
    let other = md5_hex(b"a newer version");
    let s = setup(vec![
        // 1: the store has the version of the server, but a page waits for OCR.
        (look(NOTE), entry()),
        // 2: every page is done, but the server has a newer version.
        (look(NOTE), entry_of(json!(other), "file")),
        // 3: every page is done and the md5 is the same.
        (look(NOTE), entry()),
        // 4: the server does not answer as it should: only the pages decide.
        (
            look(NOTE),
            Act::Answer(answer("500 Internal Server Error", "{}")),
        ),
    ]);
    let md5 = md5_hex(BYTES);
    s.store(&stored(
        &md5,
        vec![
            page("P1", 0, "done", vec![block("b1", "x")]),
            page("P2", 1, "pending", vec![]),
        ],
    ));
    let view = s.review.open(NOTE).unwrap();
    assert!(view.catching_up);
    assert_eq!(view.pages[1].state, PageState::Pending);

    s.store(&stored(
        &md5,
        vec![page("P1", 0, "done", vec![block("b1", "x")])],
    ));
    assert!(s.review.open(NOTE).unwrap().catching_up, "a newer version");
    assert!(!s.review.open(NOTE).unwrap().catching_up, "all caught up");
    assert!(
        !s.review.open(NOTE).unwrap().catching_up,
        "no answer: the pages decide"
    );
}

#[test]
fn an_md5_that_is_no_md5_never_becomes_a_path() {
    let bad = [
        json!("../../x"),
        json!("../../../../tmp/evil"),
        json!(null),
        json!(md5_hex(BYTES).to_uppercase()),
        json!(&md5_hex(BYTES)[..31]),
        json!(format!("{}0", md5_hex(BYTES))),
        json!(format!("{}/", &md5_hex(BYTES)[..31])),
    ];
    let mut routes: Vec<(String, Act)> = bad
        .iter()
        .map(|md5| (look(NOTE), entry_of(md5.clone(), "file")))
        .collect();
    // A folder with a valid md5 is no note either.
    routes.push((look(NOTE), entry_of(json!(md5_hex(BYTES)), "folder")));
    let s = setup(routes);
    for md5 in &bad {
        assert_eq!(s.review.page_image(NOTE, 0).unwrap_err(), NO_MD5, "{md5}");
    }
    assert_eq!(
        s.review.page_image(NOTE, 0).unwrap_err(),
        "This is a folder, not a file."
    );
    // No bytes were asked for, nothing was rendered, and nothing was made in the cache's
    // folder or next to it.
    assert!(
        s.server
            .received()
            .iter()
            .all(|r| !r.target.contains("/files/"))
    );
    assert!(s.calls().is_empty());
    assert!(s.cached().is_empty(), "{:?}", s.cached());
    let beside: Vec<_> = fs::read_dir(s.root.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .filter(|n| n != "cache")
        .collect();
    assert!(beside.is_empty(), "{beside:?}");
}

#[test]
fn a_note_whose_bytes_do_not_match_the_md5_is_not_rendered() {
    let md5 = md5_hex(BYTES);
    let s = setup(vec![
        (look(NOTE), entry()),
        // Other bytes of the same size under the ETag of the entry.
        (
            FILE.to_owned(),
            Act::Raw(file_answer(
                "200 OK",
                &[("etag", &format!("\"{md5}\""))],
                b"the bytes of the NOTE",
            )),
        ),
        (look(NOTE), entry()),
        (FILE.to_owned(), bytes_of(BYTES)),
        (look(NOTE), entry()),
        (FILE.to_owned(), bytes_of(BYTES)),
    ]);
    assert_eq!(s.review.page_image(NOTE, 0).unwrap_err(), NOT_WHOLE);
    assert!(s.calls().is_empty(), "nothing is rendered");
    assert_eq!(s.cached(), vec!["notes/"], "no note and no temp file");

    // The right bytes are cached and rendered.
    assert_eq!(
        s.review.page_image(NOTE, 0).unwrap(),
        data_url(&png_of(1, BYTES))
    );
    let note = s.cache().join("notes").join(format!("{md5}.note"));
    assert_eq!(fs::read(&note).unwrap(), BYTES);

    // A cached note that changed on disk since it was checked (in an earlier run) is checked
    // again before it is used: its bytes go and the right ones are fetched.
    let review = s.review.clone();
    let fresh = Review::new(
        review.library(),
        review.notes(),
        sidecar(s.bin.path()),
        s.cache(),
        Arc::new(|| 0),
        CACHE_LIMIT,
    );
    fs::write(&note, b"damaged").unwrap();
    assert_eq!(
        fresh.page_image(NOTE, 1).unwrap(),
        data_url(&png_of(2, BYTES))
    );
    assert_eq!(fs::read(&note).unwrap(), BYTES);
    fresh.join();
}

#[test]
fn a_page_is_rendered_alone() {
    let md5 = md5_hex(BYTES);
    let s = setup(vec![
        (look(NOTE), entry()),
        (FILE.to_owned(), bytes_of(BYTES)),
        (look(NOTE), entry()),
        (look(NOTE), entry()),
        (look(NOTE), entry()),
    ]);
    // Page index 4 is page 5 of the note.
    assert_eq!(
        s.review.page_image(NOTE, 4).unwrap(),
        data_url(&png_of(5, BYTES))
    );
    let calls = s.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].contains(" --page 5 "), "{calls:?}");
    assert_eq!(calls[0].matches("--page").count(), 1, "{calls:?}");
    assert_eq!(
        s.cached(),
        vec![
            "notes/".to_owned(),
            format!("notes/{md5}.note"),
            "pages/".to_owned(),
            format!("pages/{md5}/"),
            format!("pages/{md5}/4.png"),
        ]
    );
    // Asked again, the page comes from the cache: no render, no download.
    assert_eq!(
        s.review.page_image(NOTE, 4).unwrap(),
        data_url(&png_of(5, BYTES))
    );
    assert_eq!(s.calls().len(), 1);

    // A page that fails says why, and nothing is cached for it.
    s.flag("fail", "");
    let error = s.review.page_image(NOTE, 6).unwrap_err();
    assert!(
        error.contains("page 7: the note has 1 page and pages count from 1"),
        "{error}"
    );
    assert!(!s.cache().join("pages").join(&md5).join("6.png").exists());
    let downloads = s
        .server
        .received()
        .iter()
        .filter(|r| r.target.contains("/files/"))
        .count();
    assert_eq!(downloads, 1, "the note is downloaded once");
    // No temp folder is left.
    assert!(
        s.cached().iter().all(|f| !f.contains(".tmp-")),
        "{:?}",
        s.cached()
    );
}

#[test]
fn two_calls_for_one_page_start_one_render() {
    let md5 = md5_hex(BYTES);
    let s = setup(vec![
        (look(NOTE), entry()),
        (FILE.to_owned(), bytes_of(BYTES)),
        (look(NOTE), entry()),
    ]);
    s.flag("hold", "");
    let first = {
        let review = s.review.clone();
        std::thread::spawn(move || review.page_image(NOTE, 2))
    };
    s.wait_until("the first render", || s.calls().len() == 1);
    let second = {
        let review = s.review.clone();
        std::thread::spawn(move || review.page_image(NOTE, 2))
    };
    // The second call joined the running render before it ends.
    s.wait_until("the second call to join", || s.review.joined() == 1);
    s.flag("release", "");
    let want = data_url(&png_of(3, BYTES));
    assert_eq!(first.join().unwrap().unwrap(), want);
    assert_eq!(second.join().unwrap().unwrap(), want);
    assert_eq!(s.calls().len(), 1, "one render: {:?}", s.calls());
    assert!(s.cache().join("pages").join(&md5).join("2.png").exists());
}

#[test]
fn a_killed_render_leaves_no_cache() {
    let md5 = md5_hex(BYTES);
    let s = setup(vec![
        (look(NOTE), entry()),
        (FILE.to_owned(), bytes_of(BYTES)),
        (look(NOTE), entry()),
        (look(NOTE), entry()),
    ]);
    // The render writes its PNG and prints the path, then hangs.
    s.flag("linger", "321");
    let call = {
        let review = s.review.clone();
        std::thread::spawn(move || review.page_image(NOTE, 0))
    };
    let tmp_png = || {
        fs::read_dir(s.cache().join("pages"))
            .into_iter()
            .flatten()
            .flatten()
            .any(|d| {
                fs::read_dir(d.path())
                    .into_iter()
                    .flatten()
                    .flatten()
                    .any(|f| f.file_name().to_string_lossy().ends_with(".png"))
            })
    };
    s.wait_until("the printed page", tmp_png);
    // Killed from outside: its page, printed and written, is no answer.
    kill_in(s.bin.path());
    let error = call.join().unwrap().unwrap_err();
    assert!(error.starts_with(NO_IMAGE), "{error}");
    assert!(gone(s.bin.path()), "{}", left_in(s.bin.path()));
    assert_eq!(
        s.cached(),
        vec![
            "notes/".to_owned(),
            format!("notes/{md5}.note"),
            "pages/".to_owned()
        ],
        "no page and no temp folder"
    );

    // A render that Quit kills, and nothing starts after Quit.
    s.flag("linger", "322");
    let call = {
        let review = s.review.clone();
        std::thread::spawn(move || review.page_image(NOTE, 1))
    };
    s.wait_until("the printed page", tmp_png);
    s.review.quit();
    assert_eq!(call.join().unwrap().unwrap_err(), QUITTING);
    assert!(gone(s.bin.path()), "{}", left_in(s.bin.path()));
    assert_eq!(s.review.page_image(NOTE, 1).unwrap_err(), QUITTING);
    assert_eq!(s.calls().len(), 2);
    s.review.join();
    assert_eq!(
        s.cached(),
        vec![
            "notes/".to_owned(),
            format!("notes/{md5}.note"),
            "pages/".to_owned()
        ]
    );
}

#[test]
fn closing_the_review_stops_its_renders() {
    let other = "NOTE/Note/Other.note";
    let md5 = md5_hex(BYTES);
    let s = setup(vec![
        (look(NOTE), entry()),
        (FILE.to_owned(), bytes_of(BYTES)),
        (look(NOTE), entry()),
        (look(other), entry()),
        (look(NOTE), entry()),
        (look(NOTE), entry()),
    ]);
    s.flag("linger", "323");
    let call = {
        let review = s.review.clone();
        std::thread::spawn(move || review.page_image(NOTE, 0))
    };
    s.wait_until("the render", || s.calls().len() == 1);
    s.review.close(NOTE);
    assert_eq!(call.join().unwrap().unwrap_err(), REVIEW_CLOSED);
    assert!(gone(s.bin.path()), "{}", left_in(s.bin.path()));
    s.review.join();
    assert!(!s.cache().join("pages").join(&md5).exists());
    assert!(
        s.cached().iter().all(|f| !f.contains(".tmp-")),
        "{:?}",
        s.cached()
    );

    // A render that another review still waits for goes on when one of them closes.
    fs::remove_file(s.bin.path().join("linger")).unwrap();
    s.flag("hold", "");
    let mine = {
        let review = s.review.clone();
        std::thread::spawn(move || review.page_image(NOTE, 1))
    };
    s.wait_until("the second render", || s.calls().len() == 2);
    let theirs = {
        let review = s.review.clone();
        std::thread::spawn(move || review.page_image(other, 1))
    };
    s.wait_until("the other review to join", || s.review.joined() == 1);
    s.review.close(NOTE);
    assert_eq!(mine.join().unwrap().unwrap_err(), REVIEW_CLOSED);
    s.flag("release", "");
    assert_eq!(theirs.join().unwrap().unwrap(), data_url(&png_of(2, BYTES)));

    // A closed review opens again: its calls work.
    assert_eq!(
        s.review.page_image(NOTE, 1).unwrap(),
        data_url(&png_of(2, BYTES))
    );

    // Closing the library window closes every review.
    fs::remove_file(s.bin.path().join("hold")).unwrap();
    s.flag("linger", "324");
    let call = {
        let review = s.review.clone();
        std::thread::spawn(move || review.page_image(NOTE, 5))
    };
    s.wait_until("the last render", || s.calls().len() == 3);
    s.review.close_all();
    assert_eq!(call.join().unwrap().unwrap_err(), REVIEW_CLOSED);
    assert!(gone(s.bin.path()), "{}", left_in(s.bin.path()));
}

/// Sets the mtime of `path` to `secs` after a fixed time.
fn age(path: &Path, secs: u64) {
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(at)
        .unwrap();
}

#[test]
fn the_cache_is_trimmed_past_its_limit() {
    assert_eq!(CACHE_LIMIT, 1024 * 1024 * 1024);
    let md5 = md5_hex(BYTES);
    let (a, b) = (md5_hex(b"a"), md5_hex(b"b"));
    let s = setup_with_limit(
        vec![(look(NOTE), entry()), (FILE.to_owned(), bytes_of(BYTES))],
        200,
    );
    let cache = s.cache();
    let put = |rel: &str, len: usize, secs: u64| {
        let path = cache.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![b'x'; len]).unwrap();
        age(&path, secs);
    };
    put(&format!("notes/{a}.note"), 60, 1);
    put(&format!("pages/{a}/0.png"), 60, 2);
    put(&format!("pages/{b}/3.png"), 60, 3);
    put(&format!("notes/{b}.note"), 60, 4);
    // Not the cache's: never counted or removed.
    put("pages/other/x.png", 500, 0);

    let png = png_of(1, BYTES);
    assert_eq!(s.review.page_image(NOTE, 0).unwrap(), data_url(&png));
    // 240 old bytes and the new note and page are past 200: the oldest go first, until the
    // cache fits.
    let new = BYTES.len() + png.len();
    assert!(new + 120 <= 200 && new + 180 > 200, "the test's sizes");
    let mut want = vec![
        "notes/".to_owned(),
        format!("notes/{b}.note"),
        format!("notes/{md5}.note"),
        "pages/".to_owned(),
        format!("pages/{b}/"),
        format!("pages/{b}/3.png"),
        format!("pages/{md5}/"),
        format!("pages/{md5}/0.png"),
        "pages/other/".to_owned(),
        "pages/other/x.png".to_owned(),
    ];
    want.sort();
    assert_eq!(s.cached(), want, "the empty folder of {a} goes too");
}

/// A render that was stopped stays in the table until its worker sees it (here: while it waits
/// for another page's download of the same note). A review opened again in that time gets a
/// render of its own, never the stopped one.
#[test]
fn a_review_opened_again_gets_its_pages() {
    let other = "NOTE/Note/Other.note";
    let (arrived, downloading) = std::sync::mpsc::channel();
    let (go, held) = std::sync::mpsc::channel();
    let file = String::from_utf8(file_answer(
        "200 OK",
        &[("etag", &format!("\"{}\"", md5_hex(BYTES)))],
        BYTES,
    ))
    .unwrap();
    let s = setup(vec![
        (look(other), entry()),
        (look(NOTE), entry()),
        (look(NOTE), entry()),
        (
            FILE.to_owned(),
            Act::Hold {
                answer: file,
                arrived,
                go: held,
            },
        ),
    ]);
    let first = {
        let review = s.review.clone();
        std::thread::spawn(move || review.page_image(other, 0))
    };
    downloading.recv_timeout(WAIT).unwrap();
    let closed = {
        let review = s.review.clone();
        std::thread::spawn(move || review.page_image(NOTE, 1))
    };
    // Its worker waits for the download of the other page.
    s.wait_until("the second render job", || s.review.running() == 2);
    s.review.close(NOTE);
    assert_eq!(closed.join().unwrap().unwrap_err(), REVIEW_CLOSED);
    let again = {
        let review = s.review.clone();
        std::thread::spawn(move || review.page_image(NOTE, 1))
    };
    s.wait_until("the new render job", || s.review.running() == 3);
    go.send(()).unwrap();
    assert_eq!(first.join().unwrap().unwrap(), data_url(&png_of(1, BYTES)));
    assert_eq!(again.join().unwrap().unwrap(), data_url(&png_of(2, BYTES)));
    s.review.join();
    let renders = s.calls();
    assert_eq!(
        renders.len(),
        2,
        "the stopped job renders nothing: {renders:?}"
    );
    assert!(
        s.cached().iter().all(|f| !f.contains(".tmp-")),
        "{:?}",
        s.cached()
    );
}

#[test]
fn the_render_always_has_out() {
    let md5 = md5_hex(BYTES);
    let s = setup(vec![
        (look(NOTE), entry()),
        (FILE.to_owned(), bytes_of(BYTES)),
        (look(NOTE), entry()),
    ]);
    s.review.page_image(NOTE, 0).unwrap();
    s.review.page_image(NOTE, 9).unwrap();
    let cache = s.cache();
    let note = cache.join("notes").join(format!("{md5}.note"));
    for (call, page) in s.calls().iter().zip([1, 10]) {
        let args: Vec<&str> = call.split(' ').collect();
        assert_eq!(args.len(), 7, "{call}");
        assert_eq!(args[0], "render");
        assert_eq!(Path::new(args[1]), note, "the note by its absolute path");
        assert_eq!(
            args[2..5],
            ["--page", &page.to_string(), "--allow-unknown-version"]
        );
        assert_eq!(args[5], "--out");
        let out = Path::new(args[6]);
        assert!(out.is_absolute(), "{call}");
        assert_eq!(out.parent(), Some(cache.join("pages").as_path()), "{call}");
        let name = out.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with(&format!("{md5}.tmp-")), "{call}");
    }
    assert_eq!(s.calls().len(), 2);
}

#[test]
fn a_note_not_in_the_store_is_not_ready_or_gone() {
    let (new, gone, folder) = (
        "NOTE/Note/New.note",
        "NOTE/Note/Gone.note",
        "NOTE/Note/Folder",
    );
    let not_found = || {
        Act::Answer(answer(
            "404 Not Found",
            &json!({"error": "not_found"}).to_string(),
        ))
    };
    let s = setup(vec![
        (look(new), entry()),
        (look(gone), not_found()),
        (look(folder), entry_of(json!(null), "folder")),
        (look(new), entry()),
        (look(gone), not_found()),
    ]);
    // A file the pipeline has not read yet.
    assert_eq!(s.review.open(new).unwrap_err(), NOT_READY);
    assert_eq!(s.review.open(gone).unwrap_err(), crate::notes::NOTE_GONE);
    assert_eq!(s.review.open(folder).unwrap_err(), crate::notes::NOTE_GONE);
    // Its pages are not asked for either: the store knows no page count.
    assert_eq!(s.review.page_image(new, 0).unwrap_err(), NOT_READY);
    assert_eq!(
        s.review.page_image(gone, 0).unwrap_err(),
        crate::notes::NOTE_GONE
    );
    assert!(s.calls().is_empty());
}

#[test]
fn a_page_past_the_end_is_refused() {
    let s = setup(vec![]);
    for index in [12, 13, usize::MAX] {
        assert_eq!(s.review.page_image(NOTE, index).unwrap_err(), NO_SUCH_PAGE);
    }
    // No entry, no bytes, no render.
    assert!(s.server.received().is_empty());
    assert!(s.calls().is_empty());
    assert!(s.cached().is_empty(), "{:?}", s.cached());
}

#[test]
fn at_most_two_renders_run_at_once_newest_first() {
    let s = setup(vec![
        (look(NOTE), entry()),
        (FILE.to_owned(), bytes_of(BYTES)),
        (look(NOTE), entry()),
        (look(NOTE), entry()),
        (look(NOTE), entry()),
    ]);
    s.flag("hold", "");
    let ask = |index: usize| {
        let review = s.review.clone();
        std::thread::spawn(move || review.page_image(NOTE, index))
    };
    let first = ask(0);
    s.wait_until("the first render", || s.calls().len() == 1);
    let second = ask(1);
    s.wait_until("the second render", || s.calls().len() == 2);
    // A third page waits for a free run.
    let third = ask(2);
    s.wait_until("the third page to queue", || s.review.queued() == 1);
    assert_eq!(s.calls().len(), 2);
    let fourth = ask(3);
    s.wait_until("the fourth page to queue", || s.review.queued() == 2);
    assert_eq!(s.calls().len(), 2);
    // One run ends: the newest request goes first.
    s.flag("release-1", "");
    assert_eq!(first.join().unwrap().unwrap(), data_url(&png_of(1, BYTES)));
    s.wait_until("the next render", || s.calls().len() == 3);
    assert!(s.calls()[2].contains(" --page 4 "), "{:?}", s.calls());
    s.flag("release", "");
    assert_eq!(second.join().unwrap().unwrap(), data_url(&png_of(2, BYTES)));
    assert_eq!(fourth.join().unwrap().unwrap(), data_url(&png_of(4, BYTES)));
    assert_eq!(third.join().unwrap().unwrap(), data_url(&png_of(3, BYTES)));
    assert!(s.calls()[3].contains(" --page 3 "), "{:?}", s.calls());
}

#[test]
fn a_page_dropped_while_it_waits_for_a_run_leaves_the_queue() {
    let s = setup(vec![
        (look(NOTE), entry()),
        (FILE.to_owned(), bytes_of(BYTES)),
        (look(NOTE), entry()),
        (look(NOTE), entry()),
    ]);
    s.flag("hold", "");
    let ask = |index: usize| {
        let review = s.review.clone();
        std::thread::spawn(move || review.page_image(NOTE, index))
    };
    let first = ask(0);
    s.wait_until("the first render", || s.calls().len() == 1);
    let second = ask(1);
    s.wait_until("the second render", || s.calls().len() == 2);
    let third = ask(2);
    s.wait_until("the third page to queue", || s.review.queued() == 1);
    // It went far out of view before a run was free.
    s.review.drop_page(NOTE, 2);
    assert_eq!(third.join().unwrap().unwrap_err(), PAGE_DROPPED);
    // Its worker leaves the queue on its own thread, while both runs are still busy.
    s.wait_until("the dropped page to leave the queue", || {
        s.review.queued() == 0
    });
    assert_eq!(s.calls().len(), 2);
    s.flag("release", "");
    assert_eq!(first.join().unwrap().unwrap(), data_url(&png_of(1, BYTES)));
    assert_eq!(second.join().unwrap().unwrap(), data_url(&png_of(2, BYTES)));
    s.review.join();
    // No render ran for it.
    assert_eq!(s.calls().len(), 2, "{:?}", s.calls());
    assert!(
        s.calls().iter().all(|call| !call.contains(" --page 3 ")),
        "{:?}",
        s.calls()
    );
}

#[test]
fn a_dropped_page_stops_its_run() {
    let md5 = md5_hex(BYTES);
    let s = setup(vec![
        (look(NOTE), entry()),
        (FILE.to_owned(), bytes_of(BYTES)),
        (look(NOTE), entry()),
    ]);
    s.flag("linger", "325");
    let call = {
        let review = s.review.clone();
        std::thread::spawn(move || review.page_image(NOTE, 4))
    };
    s.wait_until("the render", || s.calls().len() == 1);
    // Another page of the review is no reason to stop it.
    s.review.drop_page(NOTE, 3);
    s.review.drop_page(OTHER, 4);
    assert_eq!(s.review.running(), 1);
    assert!(!call.is_finished());
    s.review.drop_page(NOTE, 4);
    assert_eq!(call.join().unwrap().unwrap_err(), PAGE_DROPPED);
    assert!(gone(s.bin.path()), "{}", left_in(s.bin.path()));
    s.review.join();
    assert!(!s.cache().join("pages").join(&md5).exists());
    assert!(
        s.cached().iter().all(|f| !f.contains(".tmp-")),
        "{:?}",
        s.cached()
    );
    // The page can be asked for again.
    fs::remove_file(s.bin.path().join("linger")).unwrap();
    assert_eq!(
        s.review.page_image(NOTE, 4).unwrap(),
        data_url(&png_of(5, BYTES))
    );
}
