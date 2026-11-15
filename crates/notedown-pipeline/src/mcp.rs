//! The MCP server (spec 6): `notedown mcp`, stdio, one account per session.
//!
//! The tools read the text store of the account; only `ack` writes (a bookmark). A tool error is
//! an MCP error result (`isError`), the server keeps running. The answers are the texts of
//! [`crate::format`]: TOON for lists, a TOON head plus Markdown for a delta, the OKF document for a
//! note and short plain text for `ack` and `get_block` (spec 6.3).
//!
//! Decisions (spec points left open):
//! - `get_changes` takes exactly one start: `consumer`, `since` or `from` with `upTo`. `notes`
//!   needs a fixed range (spec 5.2a): with a bookmark it would let an `ack` pass the lines of
//!   the other notes. `summary` does not go with `notes`.
//! - `get_block` of a block gone without successors names the version that dropped it: the
//!   `seq` of the first line of its page (or the `note.deleted` line) after the newest line
//!   that holds it. `textBefore` is the text of that newest line. When no line dropped it,
//!   the note state is behind the log: that is an error that says so.
//! - `get_block` of an old key (a note keyed by its path that moved) follows the `note.moved`
//!   lines with `oldNote` to the key now, answers for that key and names the new anchor. The
//!   log lines under every old key count for the last text of a deleted block.
//! - `list_notes` and `search_notes` leave deleted notes out. `folder` keeps the notes in that
//!   folder and its subfolders; the folder of a note is its path up to the last `/`, as in the
//!   manifest.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use time::{OffsetDateTime, UtcOffset};

use crate::delta::{self, MAX_CHARS, Since, Start, heirs, parse_since, stamp};
use crate::format;
use crate::model::{Block, LineChange, NoteState, PageOcrState};
use crate::search;
use crate::store::TextStore;
// The rmcp macros name `Result` and `Error` unqualified, so the crate's own stay qualified.
use crate::Error as PipelineError;

/// The server instructions; `{slug}` is the account of the session.
fn instructions(slug: &str) -> String {
    format!(
        "This server gives the handwritten notes of one notedown account ({slug}) as text. \
The text is read from the pages by OCR. Every block of text has an anchor: \
notedown://<account>/<note>/<page>#<block>. get_changes shows what changed since the bookmark \
of a consumer; after the changes are ingested, ack moves the bookmark to upTo.\n\
Format: list-shaped results and the head of a delta arrive TOON-encoded. Indentation nests \
objects, and name[N]{{f1,f2}}: heads a uniform array with one comma-separated row per record. \
The texts of a delta follow as Markdown under headings that name the block."
    )
}

/// The account of a session: `account` when it exists, else the only account of `data` (see
/// [`crate::setup::local_accounts`]). The name is checked before any store is opened.
pub fn resolve_account(data: &Path, account: Option<&str>) -> crate::Result<String> {
    let slugs = crate::setup::local_accounts(data)?;
    match account {
        Some(slug) if slugs.iter().any(|s| s == slug) => Ok(slug.to_string()),
        Some(slug) => Err(PipelineError::Account(if slugs.is_empty() {
            format!("no account {slug} in this data folder; it has no accounts")
        } else {
            format!(
                "no account {slug} in this data folder; its accounts: {}",
                slugs.join(", ")
            )
        })),
        None => match slugs.as_slice() {
            [] => Err(PipelineError::Account(
                "this data folder has no account; create one with notedown cloud account add"
                    .into(),
            )),
            [one] => Ok(one.clone()),
            many => Err(PipelineError::Account(format!(
                "this data folder has several accounts: {}; start with --account <slug>",
                many.join(", ")
            ))),
        },
    }
}

/// Runs the server on stdin and stdout until the client closes the session.
pub fn serve_stdio(data: &Path, account: Option<&str>) -> crate::Result<()> {
    let server = NotesServer::new(data, account)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| PipelineError::Mcp(e.to_string()))?;
    runtime.block_on(async move {
        let running = server
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|e| PipelineError::Mcp(e.to_string()))?;
        running
            .waiting()
            .await
            .map_err(|e| PipelineError::Mcp(e.to_string()))?;
        Ok(())
    })
}

/// The MCP server of one account. [`serve_stdio`] runs it on stdio; tests serve it on any
/// rmcp transport.
#[derive(Clone)]
pub struct NotesServer {
    slug: String,
    store: Arc<TextStore>,
    /// The offset of dates shown to people, read once at the start.
    local: UtcOffset,
    tool_router: ToolRouter<NotesServer>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListNotesParams {
    /// Only notes in this folder and its subfolders, for example "NOTE/Note/Work".
    #[serde(default)]
    pub folder: Option<String>,
    /// Only notes changed after this: a seq (412), a date (2026-10-09), an RFC 3339 time or a
    /// duration back from now (3d, 12h).
    #[serde(default)]
    pub changed_since: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChangesParams {
    /// The consumer whose bookmark starts the range, for example "crystalline".
    #[serde(default)]
    pub consumer: Option<String>,
    /// Start after this: a seq (the upTo of the last answer), a date, an RFC 3339 time or a
    /// duration. Changes no bookmark.
    #[serde(default)]
    pub since: Option<SinceArg>,
    /// A fixed range from a manifest: the seq after which it starts. Needs upTo.
    #[serde(default)]
    pub from: Option<u64>,
    /// A fixed range from a manifest: the last seq it holds. Needs from.
    #[serde(default)]
    pub up_to: Option<u64>,
    /// true gives only the manifest of the range: notes, sizes, no text.
    #[serde(default)]
    pub summary: Option<bool>,
    /// Only these note keys. Needs from and upTo.
    #[serde(default)]
    pub notes: Option<Vec<String>>,
}

/// A `since` as a number (a seq) or as text.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum SinceArg {
    Seq(u64),
    Text(String),
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AckParams {
    /// The consumer, for example "crystalline".
    pub consumer: String,
    /// The upTo of the answer that was ingested.
    pub up_to: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NoteParams {
    /// The note key or its path, for example "NOTE/Note/Planview.note".
    pub note: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BlockParams {
    /// notedown://<account>/<note>/<page>#<block>
    pub anchor: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchParams {
    /// The words to find. Every word must occur in a block.
    pub query: String,
    /// The most hits to give, 1 or more, 20 when left out.
    #[serde(default)]
    pub limit: Option<usize>,
}

fn answer(result: crate::Result<String>) -> std::result::Result<String, String> {
    result.map_err(|e| e.to_string())
}

/// Runs the blocking file work of a tool off the async workers (the HTTP server shares them
/// with the device sync).
async fn blocking(
    work: impl FnOnce() -> crate::Result<String> + Send + 'static,
) -> std::result::Result<String, String> {
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => answer(result),
        Err(e) => Err(format!("the tool stopped: {e}")),
    }
}

#[tool_router]
impl NotesServer {
    /// The server of `account` (see [`resolve_account`]) in `data`.
    pub fn new(data: &Path, account: Option<&str>) -> crate::Result<NotesServer> {
        let slug = resolve_account(data, account)?;
        let store = TextStore::open(data, &slug)?;
        Ok(NotesServer {
            slug,
            store: Arc::new(store),
            local: notedown_cloud::localtime::local_utc_offset().unwrap_or(UtcOffset::UTC),
            tool_router: Self::tool_router(),
        })
    }

    #[tool(
        description = "The state of the text store: notes, pages with text, pending and failed \
pages, the region_missed count, the oldest kept version and the bookmarks of the consumers.",
        annotations(read_only_hint = true)
    )]
    async fn status(&self) -> std::result::Result<String, String> {
        let this = self.clone();
        blocking(move || this.status_text()).await
    }

    #[tool(
        description = "Lists the notes with key, path, title, pages, last change, OCR state and \
keywords. folder keeps the notes in that folder and its subfolders. changedSince keeps the notes \
changed after a seq, a date, an RFC 3339 time or a duration like 3d.",
        annotations(read_only_hint = true)
    )]
    async fn list_notes(
        &self,
        Parameters(p): Parameters<ListNotesParams>,
    ) -> std::result::Result<String, String> {
        let this = self.clone();
        blocking(move || this.list_text(p)).await
    }

    #[tool(
        description = "The changes of one range. Give one start: consumer (its bookmark), since \
(a seq, date, time or duration) or from with upTo (a fixed range from a manifest). summary: true \
gives only the manifest, without text. notes gives only these notes and needs from and upTo. The \
answer head has the fields from, upTo and more. Ingest, then call ack with the upTo field, and \
repeat while more is true. With since, pass the previous upTo as since.",
        annotations(read_only_hint = true)
    )]
    async fn get_changes(
        &self,
        Parameters(p): Parameters<ChangesParams>,
    ) -> std::result::Result<String, String> {
        let this = self.clone();
        blocking(move || this.changes_text(p)).await
    }

    #[tool(
        description = "Moves the bookmark of a consumer to upTo, never backwards. Call it when \
the changes up to upTo are ingested.",
        annotations(read_only_hint = false)
    )]
    async fn ack(
        &self,
        Parameters(p): Parameters<AckParams>,
    ) -> std::result::Result<String, String> {
        let this = self.clone();
        blocking(move || this.ack_text(p)).await
    }

    #[tool(
        description = "One note as an OKF Markdown document, by key or path. Every block carries \
its anchor as an HTML comment.",
        annotations(read_only_hint = true)
    )]
    async fn get_note(
        &self,
        Parameters(p): Parameters<NoteParams>,
    ) -> std::result::Result<String, String> {
        let this = self.clone();
        blocking(move || this.note_text(&p.note)).await
    }

    #[tool(
        description = "The current text of one anchor (notedown://<account>/<note>/<page>#<block>). \
A replaced block gives replacedBy with the new anchors and their text. A deleted block gives the \
version that deleted it and its textBefore.",
        annotations(read_only_hint = true)
    )]
    async fn get_block(
        &self,
        Parameters(p): Parameters<BlockParams>,
    ) -> std::result::Result<String, String> {
        let this = self.clone();
        blocking(move || this.block_text(&p.anchor)).await
    }

    #[tool(
        description = "Word search over the current text of the notes. Every word of query must \
occur as a whole word in a block; case and punctuation do not matter. Gives anchors with one \
line of context. limit is 20 when left out.",
        annotations(read_only_hint = true)
    )]
    async fn search_notes(
        &self,
        Parameters(p): Parameters<SearchParams>,
    ) -> std::result::Result<String, String> {
        let this = self.clone();
        blocking(move || this.search_text(&p.query, p.limit)).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for NotesServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("notedown", env!("CARGO_PKG_VERSION")))
            .with_instructions(instructions(&self.slug))
    }
}

/// The newest `seq` and its time per note key, and the first line of the log.
struct LogFacts {
    last: BTreeMap<String, (u64, u64)>,
    first: Option<(u64, u64)>,
    last_seq: u64,
}

/// The folder of a note path: the path up to its last `/`, as in the manifest.
fn folder_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(f, _)| f)
}

/// The OCR state of a note in one word: pending, failed or truncated when one page is, else
/// done.
fn ocr_state(note: &NoteState) -> &'static str {
    let any = |s: PageOcrState| note.pages.iter().any(|p| p.state == s);
    if any(PageOcrState::Pending) {
        "pending"
    } else if any(PageOcrState::Failed) {
        "failed"
    } else if any(PageOcrState::Truncated) {
        "truncated"
    } else {
        "done"
    }
}

/// A parsed anchor: account, note key, page id, block id.
struct Anchor<'a> {
    slug: &'a str,
    key: &'a str,
    page: &'a str,
    block: &'a str,
}

fn parse_anchor(text: &str) -> crate::Result<Anchor<'_>> {
    let invalid = || PipelineError::Invalid {
        what: "anchor",
        message: format!("{text:?}, use notedown://<account>/<note>/<page>#<block>"),
    };
    let rest = text
        .trim()
        .strip_prefix("notedown://")
        .ok_or_else(invalid)?;
    let (path, block) = rest.split_once('#').ok_or_else(invalid)?;
    let mut parts = path.split('/');
    let (Some(slug), Some(key), Some(page), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid());
    };
    if [slug, key, page, block].iter().any(|s| s.is_empty()) {
        return Err(invalid());
    }
    Ok(Anchor {
        slug,
        key,
        page,
        block,
    })
}

impl NotesServer {
    fn anchor(&self, key: &str, page: &str, block: &str) -> String {
        format!("notedown://{}/{key}/{page}#{block}", self.slug)
    }

    fn log_facts(&self) -> crate::Result<LogFacts> {
        let mut facts = LogFacts {
            last: BTreeMap::new(),
            first: None,
            last_seq: 0,
        };
        self.store.read_log(0, None, &mut |line| {
            facts.first.get_or_insert((line.seq, line.ts));
            facts.last_seq = line.seq;
            facts.last.insert(line.note, (line.seq, line.ts));
            Ok(())
        })?;
        Ok(facts)
    }

    fn status_text(&self) -> crate::Result<String> {
        let notes: Vec<NoteState> = self
            .store
            .notes()?
            .into_iter()
            .filter(|n| !n.deleted)
            .collect();
        let pages = notes.iter().flat_map(|n| &n.pages);
        let count = |s: PageOcrState| pages.clone().filter(|p| p.state == s).count();
        let with_text = pages.clone().filter(|p| !p.blocks.is_empty()).count();
        let facts = self.log_facts()?;
        let stats = self.store.stats()?;
        let consumers: Vec<Value> = self
            .store
            .consumers()?
            .into_iter()
            .map(|(name, b)| {
                json!({"name": name, "seq": b.seq, "updatedAt": stamp(b.updated_at, self.local)})
            })
            .collect();
        let status = json!({
            "account": self.slug,
            "notes": notes.len(),
            "unscoped": notes.iter().filter(|n| n.unscoped).count(),
            "pagesWithText": with_text,
            "pending": count(PageOcrState::Pending),
            "failed": count(PageOcrState::Failed),
            "truncated": count(PageOcrState::Truncated),
            "regionMissed": stats.region_missed,
            "lastSeq": facts.last_seq,
            "oldestKeptSeq": facts.first.map(|(seq, _)| seq),
            "oldestKept": facts.first.map(|(_, ts)| stamp(ts, self.local)),
            "lastRun": (stats.last_run_ms > 0).then(|| stamp(stats.last_run_ms, self.local)),
            "consumers": consumers,
        });
        Ok(format::status_text(&status))
    }

    fn list_text(&self, p: ListNotesParams) -> crate::Result<String> {
        let since = p
            .changed_since
            .as_deref()
            .map(|s| parse_since(s, OffsetDateTime::now_utc(), self.local))
            .transpose()?;
        let folder = p
            .folder
            .as_deref()
            .map(|f| f.trim().trim_matches('/').to_string());
        let facts = self.log_facts()?;
        let mut rows = Vec::new();
        for note in self.store.notes()? {
            if note.deleted {
                continue;
            }
            let dir = folder_of(&note.path);
            if let Some(f) = &folder
                && !(dir == f || dir.starts_with(&format!("{f}/")) || f.is_empty())
            {
                continue;
            }
            let last = facts.last.get(&note.key).copied();
            let changed = match (&since, last) {
                (None, _) => true,
                (Some(_), None) => false,
                (Some(Since::Seq(n)), Some((seq, _))) => seq > *n,
                (Some(Since::Time(t)), Some((_, ts))) => ts >= *t,
            };
            if !changed {
                continue;
            }
            rows.push(json!({
                "key": note.key,
                "path": note.path,
                "title": note.title,
                "pages": note.pages.len(),
                "lastChange": last.map(|(_, ts)| stamp(ts, self.local)),
                "lastSeq": last.map(|(seq, _)| seq),
                "ocr": if note.unscoped { "unscoped" } else { ocr_state(&note) },
                "keywords": (!note.meta.keywords.is_empty()).then(|| note.meta.keywords.join(", ")),
            }));
        }
        Ok(format::notes_text(&rows))
    }

    fn changes_text(&self, p: ChangesParams) -> crate::Result<String> {
        let invalid = |message: &str| PipelineError::Invalid {
            what: "get_changes",
            message: message.into(),
        };
        let range = match (p.from, p.up_to) {
            (Some(from), Some(up_to)) => Some(Start::Range { from, up_to }),
            (None, None) => None,
            _ => return Err(invalid("from and upTo go together")),
        };
        let starts = usize::from(p.consumer.is_some())
            + usize::from(p.since.is_some())
            + usize::from(range.is_some());
        if starts != 1 {
            return Err(invalid(
                "give one start: consumer, since, or from with upTo",
            ));
        }
        let start = match (p.consumer, p.since, range) {
            (Some(c), _, _) => Start::Bookmark(c),
            (_, Some(SinceArg::Seq(n)), _) => Start::Since(Since::Seq(n)),
            (_, Some(SinceArg::Text(s)), _) => {
                Start::Since(parse_since(&s, OffsetDateTime::now_utc(), self.local)?)
            }
            (_, _, Some(r)) => r,
            _ => unreachable!("one start is given"),
        };
        if p.notes.is_some() && !matches!(start, Start::Range { .. }) {
            return Err(invalid(
                "notes needs a fixed range: give from and upTo from the manifest",
            ));
        }
        if p.summary == Some(true) {
            if p.notes.is_some() {
                return Err(invalid("summary and notes do not go together"));
            }
            let manifest = delta::manifest(&self.store, &start, self.local)?;
            return Ok(format::manifest_text(&manifest));
        }
        let d = delta::delta(
            &self.store,
            &start,
            p.notes.as_deref(),
            MAX_CHARS,
            self.local,
        )?;
        Ok(format::delta_text(&d))
    }

    fn ack_text(&self, p: AckParams) -> crate::Result<String> {
        let b = self.store.ack(&p.consumer, p.up_to)?;
        let mut out = format!("the bookmark of {} is at seq {}", p.consumer, b.seq);
        if b.seq < p.up_to {
            out.push_str(&format!(
                " (the log ends at seq {})",
                self.store.last_seq()?
            ));
        }
        Ok(out)
    }

    /// The note of a key or a path (also without `.note`).
    fn find_note(&self, note: &str) -> crate::Result<NoteState> {
        let note = note.trim();
        if notedown_cloud::pages::valid_key(note)
            && let Some(state) = self.store.note(note)?
        {
            return Ok(state);
        }
        let with_suffix = format!("{note}.note");
        let mut found: Vec<NoteState> = self
            .store
            .notes()?
            .into_iter()
            .filter(|n| n.path == note || n.path == with_suffix)
            .collect();
        // A live note before a deleted one at the same path.
        found.sort_by_key(|n| n.deleted);
        found.into_iter().next().ok_or_else(|| {
            PipelineError::Read(format!("no note {note:?} in account {}", self.slug))
        })
    }

    fn note_text(&self, note: &str) -> crate::Result<String> {
        let state = self.find_note(note)?;
        if state.deleted {
            return Err(PipelineError::Read(format!(
                "the note {} was deleted; get_changes shows its last text",
                state.path
            )));
        }
        Ok(format::note_document(&state, &self.slug))
    }

    fn block_text(&self, anchor: &str) -> crate::Result<String> {
        let a = parse_anchor(anchor)?;
        if a.slug != self.slug {
            return Err(PipelineError::Read(format!(
                "this session serves account {}, the anchor names account {}",
                self.slug, a.slug
            )));
        }
        if !notedown_cloud::pages::valid_key(a.key) {
            return Err(PipelineError::Invalid {
                what: "anchor",
                message: format!("{:?} is no note key", a.key),
            });
        }
        // A note keyed by its path gets a new key when it moves: follow the moves to its key now.
        let keys = self.key_chain(a.key)?;
        let key = keys.last().map_or(a.key, String::as_str);
        let answer = self.block_answer(key, &keys, a.page, a.block)?;
        if key == a.key {
            return Ok(answer);
        }
        Ok(format!(
            "the note moved: {} is now {}.\n{answer}",
            self.anchor(a.key, a.page, a.block),
            self.anchor(key, a.page, a.block)
        ))
    }

    /// `key` and every key it got by later moves (`note.moved` with `oldNote`), in log order.
    fn key_chain(&self, key: &str) -> crate::Result<Vec<String>> {
        let mut keys = vec![key.to_string()];
        self.store.read_log(0, None, &mut |line| {
            if line.change == LineChange::NoteMoved
                && line.old_note.as_deref() == keys.last().map(String::as_str)
                && line.old_note.as_deref() != Some(line.note.as_str())
            {
                keys.push(line.note);
            }
            Ok(())
        })?;
        Ok(keys)
    }

    /// The answer of `get_block` for block `block` of note `key` (its key now); `keys` holds
    /// the keys the note had, whose log lines count too.
    fn block_answer(
        &self,
        key: &str,
        keys: &[String],
        page_id: &str,
        block: &str,
    ) -> crate::Result<String> {
        let a = Anchor {
            slug: &self.slug,
            key,
            page: page_id,
            block,
        };
        let stored = self.store.note(a.key)?;
        // A deleted note keeps its last blocks in the state; they are not current.
        if let Some(state) = stored.as_ref().filter(|s| !s.deleted) {
            // Block ids are unique within a note: look on the named page first, then on all.
            let pages = state
                .pages
                .iter()
                .enumerate()
                .filter(|(_, p)| p.page_id == a.page)
                .chain(
                    state
                        .pages
                        .iter()
                        .enumerate()
                        .filter(|(_, p)| p.page_id != a.page),
                );
            for (i, page) in pages {
                if let Some(b) = page.blocks.iter().find(|b| b.id == a.block) {
                    let here = self.anchor(a.key, &page.page_id, &b.id);
                    return Ok(format!(
                        "{here} ({}, page {}):\n{}",
                        state.title,
                        i + 1,
                        b.text.trim_end()
                    ));
                }
            }
            let all: Vec<Block> = state.pages.iter().flat_map(|p| p.blocks.clone()).collect();
            let heirs = heirs(a.block, &all, &state.successors);
            if !heirs.is_empty() {
                let mut out = format!(
                    "{} was replaced.\nreplacedBy:",
                    self.anchor(a.key, a.page, a.block)
                );
                for id in &heirs {
                    for (i, page) in state.pages.iter().enumerate() {
                        if let Some(b) = page.blocks.iter().find(|b| &b.id == id) {
                            out.push_str(&format!(
                                "\n\n{} ({}, page {}):\n{}",
                                self.anchor(a.key, &page.page_id, id),
                                state.title,
                                i + 1,
                                b.text.trim_end()
                            ));
                        }
                    }
                }
                return Ok(out);
            }
        }
        self.deleted_block(&a, keys, stored.as_ref())
    }

    /// A block that is not in the note now and has no successors: its last text from the log.
    fn deleted_block(
        &self,
        a: &Anchor<'_>,
        keys: &[String],
        state: Option<&NoteState>,
    ) -> crate::Result<String> {
        // The newest line holding the block: (seq, page id, text); then the version that
        // dropped it.
        let mut held: Option<(u64, String, String)> = None;
        let mut dropped: Option<u64> = None;
        self.store.read_log(0, None, &mut |line| {
            if !keys.contains(&line.note) {
                return Ok(());
            }
            if let Some(b) = line.blocks.iter().find(|b| b.id == a.block) {
                held = Some((
                    line.seq,
                    line.page_id.clone().unwrap_or_default(),
                    b.text.clone(),
                ));
                dropped = None;
            } else if let Some((_, page, _)) = &held
                && dropped.is_none()
                && (line.change == LineChange::NoteDeleted
                    || (line.page_id.as_deref() == Some(page.as_str())
                        && matches!(
                            line.change,
                            LineChange::New | LineChange::Edited | LineChange::Deleted
                        )))
            {
                dropped = Some(line.seq);
            }
            Ok(())
        })?;
        let here = self.anchor(a.key, a.page, a.block);
        if let Some((seq, _, text)) = held {
            return match dropped {
                Some(version) => Ok(format!(
                    "{here} was deleted in version {version}.\ntextBefore:\n{}",
                    text.trim_end()
                )),
                // The log holds the block in its newest page version, the note state does not:
                // the state is behind the log until the pipeline repairs it.
                None => Err(PipelineError::Read(format!(
                    "{here} is not in the note state, but the newest version of its page (seq \
{seq}) still holds it; the note state is behind the log, run notedown pipeline and ask again"
                ))),
            };
        }
        let number = a
            .block
            .strip_prefix('b')
            .and_then(|n| n.parse::<u64>().ok());
        let existed = state.zip(number).is_some_and(|(s, n)| n < s.next_block);
        if existed {
            return Ok(format!("{here} was deleted; its text is no longer kept"));
        }
        Err(PipelineError::Read(format!(
            "no block {} in note {} of account {}",
            a.block, a.key, self.slug
        )))
    }

    fn search_text(&self, query: &str, limit: Option<usize>) -> crate::Result<String> {
        let hits = search::search(
            &self.store,
            &self.slug,
            query,
            limit.unwrap_or(search::DEFAULT_LIMIT),
        )?;
        let values: Vec<Value> = hits
            .iter()
            .map(|h| serde_json::to_value(h).unwrap_or(Value::Null))
            .collect();
        Ok(format::search_text(&values))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_parse_and_bad_ones_are_refused() {
        let a = parse_anchor("notedown://jordi/path:0123/P1#b3").unwrap();
        assert_eq!(
            (a.slug, a.key, a.page, a.block),
            ("jordi", "path:0123", "P1", "b3")
        );
        for bad in [
            "jordi/F1/P1#b3",
            "notedown://jordi/F1/P1",
            "notedown://jordi/F1#b3",
            "notedown://jordi/F1/P1/x#b3",
            "notedown://jordi//P1#b3",
            "notedown://jordi/F1/P1#",
        ] {
            assert!(parse_anchor(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_instructions_carry_the_toon_note() {
        let text = instructions("jordi");
        assert!(text.contains("name[N]{f1,f2}:"), "{text}");
        assert!(text.contains("TOON"), "{text}");
        assert!(text.contains("Markdown"), "{text}");
        assert!(!text.contains('\u{2014}'), "{text}");
    }
}
