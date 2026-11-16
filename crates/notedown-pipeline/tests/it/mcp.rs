//! The MCP server through rmcp's own client over an in-memory duplex pipe: the real tools on a
//! temp data folder whose text store is seeded through the store API.

use std::path::{Path, PathBuf};

use notedown_cloud::auth::Accounts;
use notedown_pipeline::mcp::{NotesServer, serve_stdio};
use notedown_pipeline::model::{Block, LineChange, LogLine};
use notedown_pipeline::store::{TextStore, WriterLock};
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

const SLUG: &str = "jordi";
const A: &str = "F20240424175151947577oMie9HNQYnTa";
const B: &str = "F20240424175151947577oMie9HNQYnTb";

pub(crate) struct Data {
    _dir: tempfile::TempDir,
    pub(crate) data: PathBuf,
}

fn block(id: &str, text: &str, replaces: &[&str]) -> Block {
    Block {
        id: id.into(),
        label: "text".into(),
        bbox: [0, 0, 10, 10],
        text: text.into(),
        replaces: replaces.iter().map(|s| s.to_string()).collect(),
    }
}

fn page(note: &str, path: &str, change: LineChange, blocks: Vec<Block>) -> LogLine {
    let mut line = LogLine::new(note, change);
    line.page_id = Some("P1".into());
    line.position = Some(0);
    line.path = Some(path.into());
    line.blocks = blocks;
    line
}

const PLANVIEW: &str = "Notes/Work/Planview.note";
const GARDEN: &str = "Notes/Home/Garden.note";

/// One account, two notes:
/// - seq 1: Planview new (b1, b2, b3)
/// - seq 2: Garden new (b1)
/// - seq 3: Planview edited: b1 edited, b2 deleted, b3 replaced by b4
/// - seq 4: Garden edited
pub(crate) fn setup(accounts: &[&str]) -> Data {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    for (i, slug) in accounts.iter().enumerate() {
        Accounts::new(&data)
            .create(slug, &format!("{slug}{i}@example.org"), "secret")
            .unwrap();
    }
    let w = WriterLock::take(&data).unwrap();
    let store = TextStore::open_writer(&data, SLUG, &w).unwrap();
    let mut lines = vec![
        page(
            A,
            PLANVIEW,
            LineChange::New,
            vec![
                block("b1", "Meeting with Anna on Friday", &[]),
                block("b2", "call Bob", &[]),
                block("b3", "Flow: A then B", &[]),
            ],
        ),
        page(
            B,
            GARDEN,
            LineChange::New,
            vec![block("b1", "Plant tomatoes in May\nand beans", &[])],
        ),
        page(
            A,
            PLANVIEW,
            LineChange::Edited,
            vec![
                block("b1", "Meeting with Anne on Friday", &[]),
                block("b4", "Flow: A then B then C on Friday", &["b3"]),
            ],
        ),
        page(
            B,
            GARDEN,
            LineChange::Edited,
            vec![block("b1", "Plant tomatoes in June\nand beans", &[])],
        ),
    ];
    store.append(&w, &mut lines).unwrap();
    // The repair builds the note states from the log, successors included.
    store.repair(&w).unwrap();
    let mut planview = store.note(A).unwrap().unwrap();
    planview.meta.keywords = vec!["work".into()];
    store.save_note(&w, &planview).unwrap();
    Data { _dir: dir, data }
}

pub(crate) type Client = RunningService<RoleClient, ()>;

pub(crate) async fn connect(data: &Path, account: Option<&str>) -> Client {
    let server = NotesServer::new(data, account).unwrap();
    let (server_end, client_end) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_end).await {
            let _ = running.waiting().await;
        }
    });
    ().serve(client_end).await.unwrap()
}

async fn call(client: &Client, tool: &'static str, args: Value) -> CallToolResult {
    let mut params = CallToolRequestParams::new(tool);
    if let Value::Object(map) = args {
        params = params.with_arguments(map);
    }
    client.call_tool(params).await.unwrap()
}

fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text of a call that must succeed.
pub(crate) async fn ok(client: &Client, tool: &'static str, args: Value) -> String {
    let result = call(client, tool, args).await;
    assert_ne!(result.is_error, Some(true), "{tool}: {}", text(&result));
    text(&result)
}

/// The text of a call that must fail as a tool error.
async fn err(client: &Client, tool: &'static str, args: Value) -> String {
    let result = call(client, tool, args).await;
    assert_eq!(result.is_error, Some(true), "{tool}: {}", text(&result));
    text(&result)
}

fn anchor(note: &str, block: &str) -> String {
    format!("notedown://{SLUG}/{note}/P1#{block}")
}

#[tokio::test]
async fn every_tool_answers() {
    let d = setup(&[SLUG]);
    let client = connect(&d.data, None).await;

    let info = client.peer_info().unwrap();
    let instructions = info.instructions.clone().unwrap_or_default();
    assert!(instructions.contains("TOON"), "{instructions}");
    assert!(instructions.contains("name[N]{f1,f2}:"), "{instructions}");
    assert!(!instructions.contains('\u{2014}'), "{instructions}");

    let tools = client.list_all_tools().await.unwrap();
    let mut names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "ack",
            "get_block",
            "get_changes",
            "get_note",
            "list_notes",
            "search_notes",
            "status"
        ]
    );
    for t in &tools {
        let read_only = t.annotations.as_ref().and_then(|a| a.read_only_hint);
        // ack moves a bookmark, get_changes with a consumer stamps its read time.
        let writes = ["ack", "get_changes"].contains(&t.name.as_ref());
        assert_eq!(read_only, Some(!writes), "{}", t.name);
        let description = t.description.as_deref().unwrap_or_default();
        assert!(!description.is_empty(), "{}", t.name);
        assert!(!description.contains('\u{2014}'), "{}", t.name);
    }

    let status = ok(&client, "status", json!({})).await;
    assert!(status.contains("notes: 2"), "{status}");
    assert!(status.contains("lastSeq: 4"), "{status}");

    let notes = ok(&client, "list_notes", json!({})).await;
    assert!(notes.contains("notes[2]{"), "{notes}");
    assert!(
        notes.contains(PLANVIEW) && notes.contains(GARDEN),
        "{notes}"
    );
    let work = ok(&client, "list_notes", json!({"folder": "Notes/Work"})).await;
    assert!(work.contains(PLANVIEW) && !work.contains(GARDEN), "{work}");
    let changed = ok(&client, "list_notes", json!({"changedSince": "3"})).await;
    assert!(
        changed.contains(GARDEN) && !changed.contains(PLANVIEW),
        "{changed}"
    );

    let delta = ok(&client, "get_changes", json!({"consumer": "crystalline"})).await;
    assert!(delta.contains("more: false"), "{delta}");
    assert!(delta.contains("\nfrom: 0\n"), "{delta}");
    assert!(delta.contains("\nupTo: 4\n"), "{delta}");
    assert!(delta.contains("Meeting with Anne on Friday"), "{delta}");
    assert!(delta.contains("### textNow"), "{delta}");

    let note = ok(&client, "get_note", json!({"note": PLANVIEW})).await;
    assert!(note.contains("## Page 1"), "{note}");
    assert!(
        note.contains(&format!("<!-- {} -->", anchor(A, "b1"))),
        "{note}"
    );
    let by_key = ok(&client, "get_note", json!({"note": A})).await;
    assert!(by_key.contains("Meeting with Anne on Friday"), "{by_key}");

    let current = ok(&client, "get_block", json!({"anchor": anchor(A, "b1")})).await;
    assert!(current.contains("Meeting with Anne on Friday"), "{current}");

    let hits = ok(&client, "search_notes", json!({"query": "anne"})).await;
    assert!(hits.contains(&anchor(A, "b1")), "{hits}");

    let acked = ok(
        &client,
        "ack",
        json!({"consumer": "crystalline", "upTo": 4}),
    )
    .await;
    assert!(acked.contains("seq 4"), "{acked}");

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn get_changes_with_summary_holds_no_text() {
    let d = setup(&[SLUG]);
    let client = connect(&d.data, None).await;
    let manifest = ok(
        &client,
        "get_changes",
        json!({"consumer": "crystalline", "summary": true}),
    )
    .await;
    assert!(manifest.contains("upTo: 4"), "{manifest}");
    assert!(
        manifest.contains(PLANVIEW) && manifest.contains(GARDEN),
        "{manifest}"
    );
    assert!(manifest.contains("Notes/Work"), "{manifest}");
    for t in ["Meeting", "tomatoes", "Flow"] {
        assert!(!manifest.contains(t), "{t} in {manifest}");
    }
    // The manifest moves no bookmark; it only keeps the time of the read.
    let store = TextStore::open(&d.data, SLUG).unwrap();
    let b = store.bookmark("crystalline").unwrap().unwrap();
    assert_eq!((b.seq, b.updated_at), (0, 0));
    assert!(b.read_at.is_some());
}

#[tokio::test]
async fn get_changes_with_notes_and_a_fixed_range() {
    let d = setup(&[SLUG]);
    let client = connect(&d.data, None).await;
    let delta = ok(
        &client,
        "get_changes",
        json!({"notes": [B], "from": 0, "upTo": 2}),
    )
    .await;
    assert!(delta.contains("Plant tomatoes in May"), "{delta}");
    // Seq 4 is newer than the range, Planview is not asked for.
    assert!(!delta.contains("June"), "{delta}");
    assert!(!delta.contains("Meeting"), "{delta}");

    // `notes` without a fixed range is refused: an ack after it would pass other notes.
    let e = err(
        &client,
        "get_changes",
        json!({"consumer": "crystalline", "notes": [B]}),
    )
    .await;
    assert!(e.contains("from"), "{e}");
    // Two starts at once are refused.
    let e = err(
        &client,
        "get_changes",
        json!({"consumer": "crystalline", "since": "2"}),
    )
    .await;
    assert!(e.contains("consumer"), "{e}");
    // A since needs no bookmark; the upTo of the last answer comes as a number.
    let since = ok(&client, "get_changes", json!({"since": "3"})).await;
    assert!(
        since.contains("June") && !since.contains("Meeting"),
        "{since}"
    );
    let number = ok(&client, "get_changes", json!({"since": 3})).await;
    assert_eq!(number, since);
}

#[tokio::test]
async fn ack_moves_the_bookmark() {
    let d = setup(&[SLUG]);
    let client = connect(&d.data, None).await;
    let first = ok(&client, "get_changes", json!({"consumer": "crystalline"})).await;
    assert!(first.contains("Meeting with Anne"), "{first}");
    let acked = ok(
        &client,
        "ack",
        json!({"consumer": "crystalline", "upTo": 3}),
    )
    .await;
    assert!(acked.contains("seq 3"), "{acked}");
    let store = TextStore::open(&d.data, SLUG).unwrap();
    assert_eq!(store.bookmark("crystalline").unwrap().unwrap().seq, 3);
    let next = ok(&client, "get_changes", json!({"consumer": "crystalline"})).await;
    assert!(next.contains("June"), "{next}");
    assert!(!next.contains("Meeting"), "{next}");
    // Never backwards.
    let back = ok(
        &client,
        "ack",
        json!({"consumer": "crystalline", "upTo": 1}),
    )
    .await;
    assert!(back.contains("seq 3"), "{back}");
}

#[tokio::test]
async fn get_block_of_a_replaced_and_of_a_deleted_anchor() {
    let d = setup(&[SLUG]);
    let client = connect(&d.data, None).await;

    let replaced = ok(&client, "get_block", json!({"anchor": anchor(A, "b3")})).await;
    assert!(replaced.contains("replacedBy"), "{replaced}");
    assert!(replaced.contains(&anchor(A, "b4")), "{replaced}");
    assert!(replaced.contains("Flow: A then B then C"), "{replaced}");

    let deleted = ok(&client, "get_block", json!({"anchor": anchor(A, "b2")})).await;
    assert!(deleted.contains("deleted in version 3"), "{deleted}");
    assert!(deleted.contains("textBefore"), "{deleted}");
    assert!(deleted.contains("call Bob"), "{deleted}");

    let unknown = err(&client, "get_block", json!({"anchor": anchor(A, "b99")})).await;
    assert!(unknown.contains("b99"), "{unknown}");
    let bad = err(
        &client,
        "get_block",
        json!({"anchor": "notedown://jordi/x"}),
    )
    .await;
    assert!(bad.contains("notedown://"), "{bad}");
}

#[tokio::test]
async fn search_notes_finds_all_words_in_a_block() {
    let d = setup(&[SLUG]);
    let client = connect(&d.data, None).await;

    let hits = ok(&client, "search_notes", json!({"query": "FRIDAY anne"})).await;
    assert!(hits.contains("hits[1]{"), "{hits}");
    assert!(hits.contains(&anchor(A, "b1")), "{hits}");
    assert!(hits.contains("Meeting with Anne on Friday"), "{hits}");

    // The context is the first line holding a query word.
    let beans = ok(&client, "search_notes", json!({"query": "beans"})).await;
    assert!(beans.contains(&anchor(B, "b1")), "{beans}");
    assert!(beans.contains("and beans"), "{beans}");
    assert!(!beans.contains("June"), "{beans}");

    // Words of two blocks, a deleted block and an old version never match.
    for q in ["anne flow", "bob", "anna", "may"] {
        let none = ok(&client, "search_notes", json!({"query": q})).await;
        assert!(!none.contains("notedown://"), "{q}: {none}");
    }

    // Whole words only; punctuation does not count.
    let meet = ok(&client, "search_notes", json!({"query": "meet"})).await;
    assert!(!meet.contains("notedown://"), "{meet}");
    let flow = ok(&client, "search_notes", json!({"query": "flow: c"})).await;
    assert!(flow.contains(&anchor(A, "b4")), "{flow}");

    let both = ok(&client, "search_notes", json!({"query": "friday"})).await;
    assert!(both.contains("hits[2]{"), "{both}");
    let limited = ok(
        &client,
        "search_notes",
        json!({"query": "friday", "limit": 1}),
    )
    .await;
    assert!(limited.contains("hits[1]{"), "{limited}");
    let zero = err(
        &client,
        "search_notes",
        json!({"query": "friday", "limit": 0}),
    )
    .await;
    assert!(zero.contains("limit"), "{zero}");

    let empty = err(&client, "search_notes", json!({"query": "  "})).await;
    assert!(empty.contains("query"), "{empty}");
}

#[tokio::test]
async fn a_tool_error_keeps_the_server_running() {
    let d = setup(&[SLUG]);
    let client = connect(&d.data, None).await;
    let foreign = format!("notedown://anna/{A}/P1#b1");
    let e = err(&client, "get_block", json!({"anchor": foreign})).await;
    assert!(e.contains("anna") && e.contains(SLUG), "{e}");
    let e = err(&client, "get_note", json!({"note": "Notes/Nowhere.note"})).await;
    assert!(e.contains("Notes/Nowhere.note"), "{e}");
    let e = err(&client, "get_changes", json!({})).await;
    assert!(e.contains("consumer"), "{e}");
    let status = ok(&client, "status", json!({})).await;
    assert!(status.contains("notes: 2"), "{status}");
}

#[tokio::test]
async fn the_account_is_required_with_two_accounts() {
    let d = setup(&["anna", SLUG]);
    let want = "this data folder has several accounts: anna, jordi; start with --account <slug>";
    let e = NotesServer::new(&d.data, None).err().unwrap();
    assert_eq!(e.to_string(), want);
    // serve_stdio stops before it touches stdin or stdout.
    let e = serve_stdio(&d.data, None).err().unwrap();
    assert_eq!(e.to_string(), want);
    let e = NotesServer::new(&d.data, Some("nobody")).err().unwrap();
    assert!(e.to_string().contains("nobody"), "{e}");
    assert!(!d.data.join("pipeline/nobody").exists());

    let client = connect(&d.data, Some(SLUG)).await;
    let status = ok(&client, "status", json!({})).await;
    assert!(status.contains("notes: 2"), "{status}");
    let empty = connect(&d.data, Some("anna")).await;
    let status = ok(&empty, "status", json!({})).await;
    assert!(status.contains("notes: 0"), "{status}");
}

#[test]
fn no_account_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let e = NotesServer::new(dir.path(), None).err().unwrap();
    assert!(e.to_string().contains("no account"), "{e}");
}

#[tokio::test]
async fn a_deleted_note_reads_as_deleted() {
    let d = setup(&[SLUG]);
    {
        let w = WriterLock::take(&d.data).unwrap();
        let store = TextStore::open_writer(&d.data, SLUG, &w).unwrap();
        let mut lines = [LogLine::new(B, LineChange::NoteDeleted)];
        store.append(&w, &mut lines).unwrap();
        store.repair(&w).unwrap();
        assert!(store.note(B).unwrap().unwrap().deleted);
    }
    let client = connect(&d.data, None).await;
    let block = ok(&client, "get_block", json!({"anchor": anchor(B, "b1")})).await;
    assert!(block.contains("deleted in version 5"), "{block}");
    assert!(block.contains("textBefore"), "{block}");
    assert!(block.contains("June"), "{block}");
    let beans = ok(&client, "search_notes", json!({"query": "beans"})).await;
    assert!(!beans.contains("notedown://"), "{beans}");
    let notes = ok(&client, "list_notes", json!({})).await;
    assert!(
        notes.contains(PLANVIEW) && !notes.contains(GARDEN),
        "{notes}"
    );
    let status = ok(&client, "status", json!({})).await;
    assert!(status.contains("notes: 1"), "{status}");
    let e = err(&client, "get_note", json!({"note": GARDEN})).await;
    assert!(e.contains("deleted"), "{e}");
}

#[tokio::test]
async fn get_block_follows_a_re_keyed_note() {
    const OLD: &str = "path:0123456789abcdef0123456789abcdef";
    const NEW: &str = "path:fedcba9876543210fedcba9876543210";
    let d = setup(&[SLUG]);
    {
        let w = WriterLock::take(&d.data).unwrap();
        let store = TextStore::open_writer(&d.data, SLUG, &w).unwrap();
        let mut moved = LogLine::new(NEW, LineChange::NoteMoved);
        moved.old_note = Some(OLD.into());
        moved.path = Some("Notes/Work/Budget.note".into());
        moved.old_path = Some("Notes/Budget.note".into());
        let mut lines = vec![
            // seq 5
            page(
                OLD,
                "Notes/Budget.note",
                LineChange::New,
                vec![
                    block("b1", "Budget for Q3", &[]),
                    block("b2", "an old idea", &[]),
                ],
            ),
            // seq 6
            moved,
            // seq 7
            page(
                NEW,
                "Notes/Work/Budget.note",
                LineChange::Edited,
                vec![block("b1", "Budget for Q3", &[])],
            ),
        ];
        store.append(&w, &mut lines).unwrap();
        store.repair(&w).unwrap();
        assert!(store.note(OLD).unwrap().is_none());
        assert!(store.note(NEW).unwrap().is_some());
    }
    let client = connect(&d.data, None).await;
    let current = ok(&client, "get_block", json!({"anchor": anchor(OLD, "b1")})).await;
    assert!(current.contains("the note moved"), "{current}");
    assert!(current.contains(&anchor(NEW, "b1")), "{current}");
    assert!(current.contains("Budget for Q3"), "{current}");

    let gone = ok(&client, "get_block", json!({"anchor": anchor(OLD, "b2")})).await;
    assert!(gone.contains(&anchor(NEW, "b2")), "{gone}");
    assert!(gone.contains("deleted in version 7"), "{gone}");
    assert!(gone.contains("an old idea"), "{gone}");

    // The new key answers without the move note.
    let new = ok(&client, "get_block", json!({"anchor": anchor(NEW, "b1")})).await;
    assert!(
        !new.contains("moved") && new.contains("Budget for Q3"),
        "{new}"
    );
}

#[tokio::test]
async fn a_state_behind_the_log_is_an_error_not_a_deletion() {
    let d = setup(&[SLUG]);
    {
        let w = WriterLock::take(&d.data).unwrap();
        let store = TextStore::open_writer(&d.data, SLUG, &w).unwrap();
        // A crash between the log line and the note state: no save, no repair.
        let mut lines = [page(
            B,
            GARDEN,
            LineChange::Edited,
            vec![
                block("b1", "Plant tomatoes in June\nand beans", &[]),
                block("b9", "water daily", &[]),
            ],
        )];
        store.append(&w, &mut lines).unwrap();
    }
    let client = connect(&d.data, None).await;
    let e = err(&client, "get_block", json!({"anchor": anchor(B, "b9")})).await;
    assert!(e.contains("seq 5") && e.contains("behind"), "{e}");
    assert!(!e.contains("was deleted"), "{e}");
}
