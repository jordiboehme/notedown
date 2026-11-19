use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;
#[cfg(feature = "ocr")]
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
#[cfg(feature = "ocr")]
use notedown_convert::inputs;
#[cfg(feature = "ocr")]
use notedown_convert::links::FileIndex;
#[cfg(feature = "ocr")]
use notedown_convert::pipeline::{Options, Outcome, convert_one};
use notedown_note::{Notebook, RenderOptions, VersionPolicy, encode_png};
#[cfg(feature = "ocr")]
use notedown_ocr::model;
#[cfg(feature = "ocr")]
use notedown_ocr::{OcrEngine, OcrError, OcrOptions, PageOcr, VisionEngine};

#[derive(Parser)]
#[command(
    name = "notedown",
    version,
    about = "Turn Supernote handwritten notes into Markdown with a local OCR model"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Convert .note and .png files (or folders of them) to Markdown
    #[cfg(feature = "ocr")]
    Convert(ConvertArgs),
    /// Render the pages of a .note file as PNG images, or as one PDF with --pdf
    Render(RenderArgs),
    /// Manage the OCR model files
    #[cfg(feature = "ocr")]
    Model {
        #[command(subcommand)]
        action: ModelAction,
    },
    /// Run the Supernote Private Cloud server (--refuse-sync: capture only, no files are exchanged)
    Serve(ServeArgs),
    /// The Supernote cloud server and its data folder
    Cloud {
        #[command(subcommand)]
        action: CloudAction,
    },
    /// Read the pages of the notes in a data folder as text (OCR) and keep the text store for
    /// agents. Runs until stopped (Ctrl-C). Loads the OCR model only when a page is due
    #[cfg(feature = "ocr")]
    Pipeline(PipelineArgs),
    /// The MCP server over stdio: the note texts of one account for an agent. It only reads the
    /// text store and writes bookmarks, it never runs OCR
    #[cfg(feature = "ocr")]
    Mcp(McpArgs),
    /// Log in to a Supernote Private Cloud server over REST, keep the account's token and write
    /// pipeline.toml, so the pipeline on this machine reads the server over the network
    #[cfg(feature = "ocr")]
    Connect(ConnectArgs),
}

#[cfg(feature = "ocr")]
#[derive(clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
struct PipelineArgs {
    #[command(subcommand)]
    action: Option<PipelineAction>,
    /// The data folder (default: $NOTEDOWN_DATA, else ~/notedown-data)
    #[arg(long, value_name = "DIR")]
    data: Option<PathBuf>,
    /// Read every page that is due now, without waiting for the settle time, then stop
    #[arg(long)]
    once: bool,
    /// Read the notes in scope again (for example after a model upgrade), then stop. Changed
    /// text gives new versions
    #[arg(long, conflicts_with = "once")]
    reocr: bool,
    /// With --reocr: only this note, by key or path (NOTE/Note/Plan.note)
    #[arg(long, value_name = "NOTE", requires = "reocr")]
    note: Option<String>,
    /// Stop when the process PID is no longer the parent (for the Mac app)
    #[arg(long, value_name = "PID", hide = true, conflicts_with_all = ["once", "reocr"])]
    exit_with_parent: Option<u32>,
    /// Keep each note with text of account SLUG as a Markdown file in DIR (wins over
    /// [pipeline.markdown_dirs] in the config). Repeat it for more accounts. DIR alone is for
    /// the only account the pipeline serves
    #[arg(long, value_name = "[SLUG=]DIR")]
    markdown_dir: Vec<String>,
    /// Unload the OCR model after this long without work, like 90s, 5m or 2h (wins over
    /// model_idle of [pipeline] in the config; default 5m)
    #[arg(long, value_name = "DURATION")]
    model_idle: Option<String>,
}

#[cfg(feature = "ocr")]
#[derive(Subcommand)]
enum PipelineAction {
    /// List the bookmarks of the consumers per account as JSON, or remove one
    Consumers {
        /// The data folder (default: $NOTEDOWN_DATA, else ~/notedown-data)
        #[arg(long, value_name = "DIR")]
        data: Option<PathBuf>,
        /// Only this account (to remove a bookmark: needed when there are several accounts)
        #[arg(long)]
        account: Option<String>,
        /// Remove the bookmark of this consumer
        #[arg(long, value_name = "NAME")]
        remove: Option<String>,
    },
    /// Print the changes of the text store as JSON: the same answers as get_changes of the MCP
    /// server. A consumer's bookmark does not move, use ack for that
    Changes(ChangesArgs),
    /// Print the status of one account as JSON: whether the pipeline runs and the page it
    /// reads, the pages with text, pending, queued, waiting and failed, the last error and the
    /// consumers with their last ack, last read and notes behind. Creates and changes nothing.
    /// Each call reads the whole text log, so do not ask more often than every few seconds
    Status {
        /// The data folder (default: $NOTEDOWN_DATA, else ~/notedown-data)
        #[arg(long, value_name = "DIR")]
        data: Option<PathBuf>,
        /// The account slug (can be left out when the data folder has only one account)
        #[arg(long)]
        account: Option<String>,
    },
    /// Print the text blocks that hold every word of --query as one JSON array: per block its
    /// anchor, note title, page and one line of context. Words are runs of letters and digits,
    /// compared case-insensitive. Creates and changes nothing
    Search {
        /// The data folder (default: $NOTEDOWN_DATA, else ~/notedown-data)
        #[arg(long, value_name = "DIR")]
        data: Option<PathBuf>,
        /// The account slug (can be left out when the data folder has only one account)
        #[arg(long)]
        account: Option<String>,
        /// The words to find. Give a text that starts with `-` as --query=TEXT
        #[arg(long, value_name = "TEXT")]
        query: String,
        /// The most blocks to print
        #[arg(long, value_name = "N", default_value_t = notedown_pipeline::search::DEFAULT_LIMIT)]
        limit: usize,
    },
    /// Print the Markdown of one note: the same text as its file in the Markdown folder and
    /// get_note of the MCP server, with every page (a page without text has its heading only).
    /// Creates and changes nothing
    Note {
        /// The data folder (default: $NOTEDOWN_DATA, else ~/notedown-data)
        #[arg(long, value_name = "DIR")]
        data: Option<PathBuf>,
        /// The account slug (can be left out when the data folder has only one account)
        #[arg(long)]
        account: Option<String>,
        /// The note key
        #[arg(long, value_name = "KEY")]
        key: String,
    },
    /// Move the bookmark of a consumer to --up-to, never backwards, and print it as JSON
    Ack {
        /// The data folder (default: $NOTEDOWN_DATA, else ~/notedown-data)
        #[arg(long, value_name = "DIR")]
        data: Option<PathBuf>,
        /// The account slug (can be left out when the data folder has only one account)
        #[arg(long)]
        account: Option<String>,
        /// The consumer name
        #[arg(long, value_name = "NAME")]
        consumer: String,
        /// The upTo of the changes that were ingested
        #[arg(long, value_name = "SEQ")]
        up_to: u64,
    },
}

#[cfg(feature = "ocr")]
#[derive(clap::Args)]
#[command(group(clap::ArgGroup::new("start").required(true).args(["consumer", "since", "from"])))]
struct ChangesArgs {
    /// The data folder (default: $NOTEDOWN_DATA, else ~/notedown-data)
    #[arg(long, value_name = "DIR")]
    data: Option<PathBuf>,
    /// The account slug (can be left out when the data folder has only one account)
    #[arg(long)]
    account: Option<String>,
    /// The changes after the bookmark of this consumer
    #[arg(long, value_name = "NAME")]
    consumer: Option<String>,
    /// The changes since a seq (412), a date (2026-10-09), a time (2026-10-09T08:00:00Z) or a
    /// duration back from now (3d). Needs no bookmark
    #[arg(long, value_name = "X")]
    since: Option<String>,
    /// A fixed range: the changes after this seq (with --up-to)
    #[arg(long, value_name = "SEQ", requires = "up_to")]
    from: Option<u64>,
    /// A fixed range: the changes up to this seq (with --from)
    #[arg(long, value_name = "SEQ", requires = "from")]
    up_to: Option<u64>,
    /// Only the list of changed notes with their sizes, no text
    #[arg(long, conflicts_with = "notes")]
    summary: bool,
    /// Only these notes, by key, comma separated (with --from and --up-to)
    #[arg(long, value_name = "KEY,...", value_delimiter = ',', requires = "from")]
    notes: Option<Vec<String>>,
}

#[cfg(feature = "ocr")]
#[derive(clap::Args)]
struct McpArgs {
    /// The data folder (default: $NOTEDOWN_DATA, else ~/notedown-data)
    #[arg(long, value_name = "DIR")]
    data: Option<PathBuf>,
    /// The account slug (can be left out when the data folder has only one account)
    #[arg(long)]
    account: Option<String>,
}

#[derive(clap::Args)]
struct ServeArgs {
    /// The data folder (server.toml, accounts, logs); default: $NOTEDOWN_DATA, else ~/notedown-data
    #[arg(long, value_name = "DIR")]
    data: Option<PathBuf>,
    /// Also write every request and frame, masked, to <data>/capture/ (this run only)
    #[arg(long)]
    capture: bool,
    /// Refuse every sync, whatever server.toml says: capture only, no files are exchanged (this
    /// run only)
    #[arg(long)]
    refuse_sync: bool,
    /// Also run `notedown pipeline` for the same data folder. It stops when the server stops
    #[arg(long)]
    #[cfg(feature = "ocr")]
    pipeline: bool,
    /// Stop when the process PID is no longer the parent (for the Mac app)
    #[arg(long, value_name = "PID", hide = true)]
    exit_with_parent: Option<u32>,
}

#[derive(Subcommand)]
enum CloudAction {
    /// Manage the accounts of a data folder
    Account {
        /// The data folder (default: $NOTEDOWN_DATA, else ~/notedown-data)
        #[arg(long, value_name = "DIR")]
        data: Option<PathBuf>,
        #[command(subcommand)]
        action: AccountAction,
    },
    /// Read the change journal or write a snapshot for a new consumer
    Journal {
        #[command(subcommand)]
        action: JournalAction,
    },
    /// Fill an account with a device backup and open its protect gate
    Import(ImportArgs),
    /// Fill an account's digests from the device's .snbak backup and open digest and to-do sync.
    /// To-dos are never touched. A digest or digest group that never reached a cloud is left
    /// out: the device adds it itself at its first sync
    ImportAppData(ImportAppDataArgs),
    /// Copy one file into an account (connected devices are told to sync)
    Put(PutArgs),
    /// Copy every file of the device over Browse & Access (port 8089) into a folder
    BackupDevice(BackupDeviceArgs),
    /// Manage the recycle bin of an account
    Recycle {
        #[command(subcommand)]
        action: RecycleAction,
    },
    /// Look for files added, changed, renamed or deleted by hand in the data folder and journal
    /// them, so the device follows. The server does this by itself; run it to see the result
    /// at once, or with --accept after a hold
    Scan(ScanArgs),
    /// Write a PDF of every note into the render_dir of server.toml (default: share/ in the data
    /// folder), and move or delete the PDFs of moved or deleted notes. Safe while the server runs
    Render(CloudRenderArgs),
    /// Print a short report of what the server did and write a zip for a bug report. The zip
    /// shows no note, folder or account names by default. Safe while the server runs
    Diagnose(DiagnoseArgs),
}

#[derive(clap::Args)]
struct DiagnoseArgs {
    /// The data folder (default: $NOTEDOWN_DATA, else ~/notedown-data)
    #[arg(long, value_name = "DIR")]
    data: Option<PathBuf>,
    /// The start of the window: a date (2026-10-09), a time (2026-10-09T08:00:00Z) or a
    /// duration back from now (30m, 12h, 3d)
    #[arg(long, value_name = "TIME", default_value = "3d")]
    since: String,
    /// Only this device serial
    #[arg(long, value_name = "SERIAL")]
    device: Option<String>,
    /// Only this account
    #[arg(long)]
    account: Option<String>,
    /// Keep the real names in the zip, for your own use. Do not share such a zip
    #[arg(long)]
    private: bool,
    /// The zip to write (default: notedown-diag-<time>.zip in the current folder)
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,
}

#[derive(clap::Args)]
struct CloudRenderArgs {
    /// The data folder (default: $NOTEDOWN_DATA, else ~/notedown-data)
    #[arg(long, value_name = "DIR")]
    data: Option<PathBuf>,
    /// Only this account (default: every account)
    #[arg(long)]
    account: Option<String>,
    /// Render every note again, also those that didn't change
    #[arg(long)]
    all: bool,
}

#[derive(clap::Args)]
struct ScanArgs {
    /// The data folder (default: $NOTEDOWN_DATA, else ~/notedown-data)
    #[arg(long, value_name = "DIR")]
    data: Option<PathBuf>,
    /// The account slug (default: every account, one after the other)
    #[arg(long)]
    account: Option<String>,
    /// Print what the scan would journal, change nothing
    #[arg(long)]
    dry_run: bool,
    /// Apply the changes also when they would leave fewer than scan_min_files files, and lift
    /// the hold
    #[arg(long)]
    accept: bool,
}

#[derive(Subcommand)]
enum RecycleAction {
    /// Remove old versions and deleted files by the recycle_keep_* settings of server.toml.
    /// Works also when recycle_prune is off (that only stops the hourly prune of the server)
    Prune(RecyclePruneArgs),
}

#[derive(clap::Args)]
struct RecyclePruneArgs {
    /// The data folder (default: $NOTEDOWN_DATA, else ~/notedown-data)
    #[arg(long, value_name = "DIR")]
    data: Option<PathBuf>,
    /// The account slug
    #[arg(long)]
    account: String,
    /// Print what would be removed, change nothing
    #[arg(long)]
    dry_run: bool,
}

#[derive(clap::Args)]
struct ImportArgs {
    /// The backup folder (Note, Document, MyStyle, EXPORT, SCREENSHOT, INBOX)
    #[arg(value_name = "BACKUP DIR")]
    backup: PathBuf,
    /// The data folder (default: $NOTEDOWN_DATA, else ~/notedown-data)
    #[arg(long, value_name = "DIR")]
    data: Option<PathBuf>,
    /// The account slug
    #[arg(long)]
    account: String,
    /// Move the files the account holds now to the recycle bin first
    #[arg(long)]
    replace: bool,
    /// Print the mapping and the counts, write nothing
    #[arg(long)]
    dry_run: bool,
    /// Accept a backup without backup.json (a copy made another way, for example over USB).
    /// Make sure the copy is complete: a missing file makes the device delete its own copy.
    #[arg(long)]
    no_report: bool,
    /// Accept a backup without any files (the device is really empty)
    #[arg(long)]
    allow_empty: bool,
    /// Print JSON lines on stdout (progress lines and a last result line)
    #[arg(long)]
    json: bool,
    /// Stop when the process with this id is no longer the parent (the Mac app passes its own)
    #[arg(long, value_name = "PID", hide = true)]
    exit_with_parent: Option<u32>,
}

#[derive(clap::Args)]
struct ImportAppDataArgs {
    /// The device backup (.snbak, made by the device's own backup function)
    #[arg(value_name = "SNBAK", required_unless_present = "empty")]
    snbak: Option<PathBuf>,
    /// The data folder (default: $NOTEDOWN_DATA, else ~/notedown-data)
    #[arg(long, value_name = "DIR")]
    data: Option<PathBuf>,
    /// The account slug
    #[arg(long)]
    account: String,
    /// Import nothing and only open digest and to-do sync (the device has no digests)
    #[arg(long, conflicts_with = "snbak")]
    empty: bool,
    /// Clear the digests the account holds now first (to-dos stay as they are)
    #[arg(long)]
    replace: bool,
    /// Print the mapping and the counts, write nothing
    #[arg(long)]
    dry_run: bool,
    /// Print JSON lines on stdout (progress lines and a last result line)
    #[arg(long)]
    json: bool,
    /// Stop when the process with this id is no longer the parent (the Mac app passes its own)
    #[arg(long, value_name = "PID", hide = true)]
    exit_with_parent: Option<u32>,
}

#[derive(clap::Args)]
struct PutArgs {
    /// The file to copy
    file: PathBuf,
    /// The data folder (default: $NOTEDOWN_DATA, else ~/notedown-data)
    #[arg(long, value_name = "DIR")]
    data: Option<PathBuf>,
    /// The account slug
    #[arg(long)]
    account: String,
    /// The wire folder, inside NOTE/Note, NOTE/MyStyle, DOCUMENT/Document, EXPORT, SCREENSHOT or
    /// INBOX (for example DOCUMENT/Document/Inbox)
    #[arg(long, value_name = "WIRE FOLDER")]
    to: String,
    /// The file name in the store (default: the name of the file)
    #[arg(long)]
    name: Option<String>,
}

#[derive(clap::Args)]
struct BackupDeviceArgs {
    /// The Browse & Access address of the device, for example http://192.168.1.20:8089
    url: String,
    /// The folder to write into (empty or missing, unless --resume)
    dir: PathBuf,
    /// Also copy REMOVABLE0 (the SD card)
    #[arg(long)]
    include_sd: bool,
    /// Continue an earlier backup in DIR; files whose size matches are kept
    #[arg(long)]
    resume: bool,
    /// The device's offset from UTC for the file dates, like +02:00 (default: this computer's)
    #[arg(long, value_name = "+HH:MM", allow_hyphen_values = true)]
    utc_offset: Option<String>,
    /// Print JSON lines on stdout: progress lines {"files","bytes","path"} and a last line
    /// {"report": ...}
    #[arg(long)]
    json: bool,
    /// Stop when the process with this id is no longer the parent (the Mac app passes its own)
    #[arg(long, value_name = "PID", hide = true)]
    exit_with_parent: Option<u32>,
}

#[derive(Subcommand)]
enum JournalAction {
    /// Print the journal events as JSON lines
    Tail {
        /// The data folder (default: $NOTEDOWN_DATA, else ~/notedown-data)
        #[arg(long, value_name = "DIR")]
        data: Option<PathBuf>,
        /// Start at this seq (default: the oldest kept event)
        #[arg(long, value_name = "SEQ")]
        from: Option<u64>,
        /// Keep running and print new events as they come (Ctrl-C stops)
        #[arg(long)]
        follow: bool,
    },
    /// Journal the current files, folders, digests and to-dos of an account as created events
    Snapshot {
        /// The data folder (default: $NOTEDOWN_DATA, else ~/notedown-data)
        #[arg(long, value_name = "DIR")]
        data: Option<PathBuf>,
        /// The account slug
        #[arg(long)]
        account: String,
    },
}

#[derive(Subcommand)]
enum AccountAction {
    /// Create an account
    Add {
        /// Folder name of the account (a-z, 0-9, - and _)
        slug: String,
        /// Email the device logs in with
        #[arg(long)]
        email: String,
        /// Read the password from the first line of stdin instead of asking for it
        #[arg(long)]
        password_stdin: bool,
    },
    /// Set a new password (also unlocks the account)
    Passwd {
        slug: String,
        /// Read the password from the first line of stdin instead of asking for it
        #[arg(long)]
        password_stdin: bool,
    },
    /// List the accounts
    List,
    /// Delete an account with all its files and its note text
    Remove {
        slug: String,
        /// Confirm the deletion
        #[arg(long)]
        yes: bool,
    },
    /// List or revoke the REST tokens of an account
    Tokens {
        #[command(subcommand)]
        action: TokensAction,
    },
}

#[derive(Subcommand)]
enum TokensAction {
    /// List the tokens (id, name and creation time, never the token itself)
    List { slug: String },
    /// Revoke one token by its id, or all of them
    Revoke {
        slug: String,
        /// The token id from `tokens list`
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        id: Option<String>,
        /// Revoke every token of the account
        #[arg(long)]
        all: bool,
    },
}

#[cfg(feature = "ocr")]
#[derive(clap::Args)]
struct ConvertArgs {
    /// Files or folders to convert (folders are searched recursively)
    #[arg(required = true, value_name = "PATH")]
    paths: Vec<PathBuf>,
    /// Write the Markdown files under this folder instead of next to the sources
    #[arg(long, value_name = "DIR")]
    out: Option<PathBuf>,
    /// Save the page images to <name>.assets/ and link them in the Markdown
    #[arg(long)]
    images: bool,
    /// Write <name>.debug.json with the raw OCR output and metadata
    #[arg(long)]
    debug: bool,
    /// Convert even when the output is up to date, and replace Markdown files notedown did not write
    #[arg(long)]
    force: bool,
    /// Keep the template (ruled lines, grid) in the page images
    #[arg(long)]
    keep_template: bool,
    /// Use this GGUF model file instead of the default
    #[arg(long, value_name = "GGUF")]
    model: Option<PathBuf>,
    /// Use this GGUF vision projector file instead of the default
    #[arg(long, value_name = "GGUF")]
    mmproj: Option<PathBuf>,
    /// Upper limit of generated tokens per page
    #[arg(long, value_name = "N", default_value_t = 4096)]
    max_tokens: u32,
    /// CPU threads for inference
    #[arg(long, value_name = "N")]
    threads: Option<i32>,
    /// Accept .note files with an unknown format version
    #[arg(long)]
    allow_unknown_version: bool,
}

#[derive(clap::Args)]
struct RenderArgs {
    /// The .note file to render
    file: PathBuf,
    /// Write the PNG files to this folder (default: next to the .note file)
    #[arg(long, value_name = "DIR")]
    out: Option<PathBuf>,
    /// Keep the template (ruled lines, grid) in the images
    #[arg(long)]
    keep_template: bool,
    /// Write one PDF with all pages instead of one PNG per page
    #[arg(long)]
    pdf: bool,
    /// Render only this page (1-based, can be repeated); a page that fails is
    /// reported on stderr and the others still run
    #[arg(long, value_name = "N", conflicts_with = "pdf")]
    page: Vec<usize>,
    /// Print the pages as one JSON line ({"pages":[{"id":..,"landscape":..}]}) and write no file
    #[arg(long, conflicts_with_all = ["pdf", "page", "out", "keep_template"])]
    info: bool,
    /// Accept .note files with an unknown format version
    #[arg(long)]
    allow_unknown_version: bool,
}

#[cfg(feature = "ocr")]
#[derive(Subcommand)]
enum ModelAction {
    /// Download the model files into the cache (continues a stopped download)
    Pull {
        /// Print one JSON line per progress update on stdout
        #[arg(long)]
        json: bool,
        /// Print the licences of the model files and stop, without a download
        #[arg(long)]
        licences_only: bool,
        /// Stop when the process PID is no longer the parent (for the Mac app)
        #[arg(long, value_name = "PID", hide = true)]
        exit_with_parent: Option<u32>,
    },
    /// Print the cache folder and the model file paths
    Path,
    /// Check size and SHA-256 of the model files
    Verify,
    /// Delete the files of the old OCR model (Chandra) from the cache
    CleanOld,
}

#[cfg(feature = "ocr")]
/// Loads the model on the first page that needs OCR, so an up-to-date folder never loads it.
struct LazyEngine {
    label: String,
    model: Option<PathBuf>,
    mmproj: Option<PathBuf>,
    opts: OcrOptions,
    engine: Option<VisionEngine>,
    load_error: Option<String>,
    loaded_in: Option<Duration>,
}

#[cfg(feature = "ocr")]
impl OcrEngine for LazyEngine {
    fn ocr_png(&mut self, png: &[u8]) -> Result<PageOcr, OcrError> {
        if self.engine.is_none() {
            if let Some(msg) = &self.load_error {
                return Err(OcrError::Engine(msg.clone()));
            }
            let start = Instant::now();
            let loaded =
                model::resolve(self.model.as_deref(), self.mmproj.as_deref()).and_then(|paths| {
                    eprintln!("loading model {} ...", paths.label);
                    VisionEngine::load(&paths, self.opts)
                });
            match loaded {
                Ok(e) => {
                    self.loaded_in = Some(start.elapsed());
                    self.engine = Some(e);
                }
                Err(e) => {
                    self.load_error = Some(e.to_string());
                    return Err(e);
                }
            }
        }
        match self.engine.as_mut() {
            Some(e) => e.ocr_png(png),
            None => Err(OcrError::Engine("model not loaded".into())),
        }
    }

    fn label(&self) -> &str {
        &self.label
    }
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

#[cfg(feature = "ocr")]
fn display_name(rel: &Path) -> String {
    rel.to_string_lossy().into_owned()
}

#[cfg(feature = "ocr")]
fn convert(args: ConvertArgs) -> Result<ExitCode> {
    let (found, warnings) = inputs::collect(&args.paths, args.out.as_deref());
    for w in &warnings {
        eprintln!("warning: {w}");
    }
    if found.is_empty() {
        eprintln!("no .note or .png files found");
        return Ok(ExitCode::from(1));
    }
    let outputs = inputs::output_paths(&found, args.out.as_deref());
    // The same name `model::resolve` gives the loaded engine.
    let label = model::label_for(args.model.as_deref());
    let mut engine = LazyEngine {
        label,
        model: args.model.clone(),
        mmproj: args.mmproj.clone(),
        opts: OcrOptions {
            max_tokens: args.max_tokens,
            threads: args.threads,
        },
        engine: None,
        load_error: None,
        loaded_in: None,
    };
    let index = FileIndex::build(&found, &outputs, args.out.as_deref());
    let opts = Options {
        out: args.out.clone(),
        images: args.images,
        debug: args.debug,
        force: args.force,
        keep_template: args.keep_template,
        allow_unknown_version: args.allow_unknown_version,
    };

    let (mut converted, mut skipped, mut failed) = (0usize, 0usize, 0usize);
    let mut headers = 0usize;
    let run_start = Instant::now();
    let mut hinted = false;
    for (input, output) in found.iter().zip(&outputs) {
        let name = display_name(&input.rel);
        let target = output
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let start = Instant::now();
        match convert_one(input, output, &mut engine, &opts, &index) {
            Ok(Outcome::Converted { pages }) => {
                converted += 1;
                eprintln!(
                    "converted {name} -> {target} ({}, {}s)",
                    plural(pages, "page"),
                    start.elapsed().as_secs()
                );
            }
            Ok(Outcome::Skipped) => {
                skipped += 1;
                eprintln!("skipped {name} (unchanged)");
            }
            Ok(Outcome::HeaderUpdated) => {
                headers += 1;
                eprintln!("updated the header of {name} (text unchanged, no OCR)");
            }
            Err(e) => {
                failed += 1;
                let reason = format!("{e:#}");
                eprintln!("failed {name}: {reason}");
                if !hinted && reason.contains("notedown model pull") {
                    hinted = true;
                    eprintln!("hint: run `notedown model pull` to download the model");
                }
            }
        }
    }
    let headers = if headers > 0 {
        format!(", {} updated", plural(headers, "header"))
    } else {
        String::new()
    };
    eprintln!(
        "done: {} converted{headers}, {} skipped, {} failed ({}s)",
        converted,
        skipped,
        failed,
        run_start.elapsed().as_secs()
    );
    Ok(if failed > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn render(args: RenderArgs) -> Result<ExitCode> {
    // The format is read from the bytes, so the file name does not matter.
    let policy = if args.allow_unknown_version {
        VersionPolicy::AllowUnknown
    } else {
        VersionPolicy::Strict
    };
    let nb = Notebook::open(&args.file, policy)
        .with_context(|| format!("cannot read {}", args.file.display()))?;
    if args.info {
        let pages: Vec<serde_json::Value> = notedown_cloud::pages::fingerprint::page_ids(&nb)
            .into_iter()
            .zip(&nb.pages)
            .map(|(id, page)| serde_json::json!({"id": id, "landscape": page.landscape}))
            .collect();
        let mut out = std::io::stdout().lock();
        writeln!(out, "{}", serde_json::json!({ "pages": pages }))?;
        out.flush()?;
        return Ok(ExitCode::SUCCESS);
    }
    let dir = match &args.out {
        Some(d) => d.clone(),
        None => args
            .file
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
    };
    std::fs::create_dir_all(&dir)?;
    let stem = args
        .file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let opts = RenderOptions {
        include_background: args.keep_template,
        ..RenderOptions::default()
    };
    if args.pdf {
        let path = dir.join(format!("{stem}.pdf"));
        let bytes = nb.render_pdf(&opts).context("cannot render the PDF")?;
        std::fs::write(&path, bytes).with_context(|| format!("cannot write {}", path.display()))?;
        println!("{}", path.display());
        return Ok(ExitCode::SUCCESS);
    }
    let pages: Vec<usize> = if args.page.is_empty() {
        (1..=nb.pages.len()).collect()
    } else {
        let mut seen = std::collections::HashSet::new();
        args.page
            .iter()
            .copied()
            .filter(|n| seen.insert(*n))
            .collect()
    };
    let mut failed = false;
    for n in pages {
        let result = (|| -> Result<PathBuf> {
            if n == 0 || n > nb.pages.len() {
                return Err(anyhow!(
                    "the note has {} and pages count from 1",
                    plural(nb.pages.len(), "page")
                ));
            }
            let img = nb.render_page(n - 1, &opts)?;
            let path = dir.join(format!("{stem}-page-{n:03}.png"));
            std::fs::write(&path, encode_png(&img)?)
                .with_context(|| format!("cannot write {}", path.display()))?;
            Ok(path)
        })();
        match result {
            Ok(path) => {
                let mut out = std::io::stdout().lock();
                writeln!(out, "{}", path.display())?;
                out.flush()?;
            }
            Err(e) => {
                eprintln!("error: page {n}: {e:#}");
                failed = true;
            }
        }
    }
    Ok(if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// One progress line of `model pull --json`.
#[cfg(feature = "ocr")]
fn progress_json(file: &str, done: u64, total: u64) -> String {
    serde_json::json!({ "file": file, "done": done, "total": total }).to_string()
}

/// The last line of `model pull --json`.
#[cfg(feature = "ocr")]
fn ready_json(path: &Path) -> String {
    serde_json::json!({ "ready": path.display().to_string() }).to_string()
}

#[cfg(feature = "ocr")]
fn model_command(action: ModelAction) -> Result<ExitCode> {
    match action {
        ModelAction::Pull {
            json,
            licences_only,
            exit_with_parent: parent,
        } => {
            // The licences come first, before anything is downloaded.
            let licences = model::licence_lines();
            if json {
                let files = model::licence_files();
                println!(
                    "{}",
                    serde_json::json!({ "licences": licences, "files": files })
                );
            } else {
                eprintln!("The model files have these licences:");
                for line in &licences {
                    eprintln!("{line}");
                }
            }
            if licences_only {
                return Ok(ExitCode::SUCCESS);
            }
            if !json {
                eprintln!("The download is {}.", model::TOTAL_SIZE);
            }
            // SIGTERM ends the pull as it is; the part file stays and the next pull continues.
            exit_with_parent(parent);
            let mut last = Instant::now() - Duration::from_secs(10);
            let mut current = String::new();
            let mut progress = |file: &str, done: u64, total: u64| {
                let new_file = file != current;
                if new_file {
                    current = file.to_string();
                }
                let finished = total > 0 && done >= total;
                let wait = Duration::from_millis(if json { 250 } else { 2000 });
                if new_file || finished || last.elapsed() >= wait {
                    last = Instant::now();
                    if json {
                        use std::io::Write;
                        let mut out = std::io::stdout().lock();
                        let _ = writeln!(out, "{}", progress_json(file, done, total));
                        let _ = out.flush();
                    } else {
                        let pct = (done * 100).checked_div(total).unwrap_or(0);
                        eprintln!("downloading {file}: {pct}%");
                    }
                }
            };
            let paths = model::pull(&mut progress)?;
            if json {
                println!("{}", ready_json(&paths.files.model));
            } else {
                eprintln!("model ready: {}", paths.files.model.display());
            }
            Ok(ExitCode::SUCCESS)
        }
        ModelAction::Path => {
            let dir = model::cache_dir()?;
            let files = model::ModelFiles::in_dir(&dir);
            println!("cache: {}", dir.display());
            println!("model: {}", files.model.display());
            println!("mmproj: {}", files.mmproj.display());
            println!("layout: {}", files.layout.display());
            println!("lines: {}", files.text_det.display());
            Ok(ExitCode::SUCCESS)
        }
        ModelAction::Verify => {
            let dir = model::cache_dir()?;
            let mut bad = false;
            for file in &model::FILES {
                match model::verify_file(&dir.join(file.local), file) {
                    Ok(()) => println!("{}: ok", file.local),
                    Err(e) => {
                        bad = true;
                        println!("{}: {e}", file.local);
                    }
                }
            }
            Ok(if bad {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            })
        }
        ModelAction::CleanOld => {
            let removed = model::clean_old(&model::cache_dir()?)?;
            if removed.is_empty() {
                println!("no old model files");
            }
            for path in removed {
                println!("removed {}", path.display());
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn read_password(from_stdin: bool, confirm: bool) -> Result<String> {
    let password = if from_stdin {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("reading the password from stdin")?;
        line.trim_end_matches(['\r', '\n']).to_string()
    } else {
        let first = rpassword::prompt_password("Password: ").context("reading the password")?;
        if confirm {
            let second =
                rpassword::prompt_password("Repeat password: ").context("reading the password")?;
            if first != second {
                return Err(anyhow!("the passwords do not match"));
            }
        }
        first
    };
    if password.is_empty() {
        return Err(anyhow!("the password must not be empty"));
    }
    Ok(password)
}

/// The data folder of a command: `--data`, else `$NOTEDOWN_DATA` when it is set and not empty,
/// else `~/notedown-data`.
fn resolve_data(
    arg: Option<PathBuf>,
    env: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(path) = arg {
        return Ok(path);
    }
    if let Some(path) = env.filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    match home.filter(|h| !h.as_os_str().is_empty()) {
        Some(home) => Ok(home.join("notedown-data")),
        None => Err(anyhow!("no data folder: pass --data or set NOTEDOWN_DATA")),
    }
}

/// `resolve_data` with the real environment.
fn data_dir(arg: Option<PathBuf>) -> Result<PathBuf> {
    resolve_data(
        arg,
        std::env::var_os("NOTEDOWN_DATA"),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

/// Resolves a command's data folder and writes it back, so the command sees the same folder.
fn pin_data(slot: &mut Option<PathBuf>) -> Result<PathBuf> {
    let data = data_dir(slot.take())?;
    *slot = Some(data.clone());
    Ok(data)
}

/// Finish or drop unfinished changes of every account (a crash of the server or of an
/// earlier command). Runs at the start of every cloud command.
fn recover_accounts(data: &Path) -> Result<()> {
    if !data.join("accounts").is_dir() {
        return Ok(());
    }
    let journal = notedown_cloud::journal::Journal::open(data)?;
    // A broken account is reported and skipped; a write to it fails later with the reason.
    for (slug, r) in notedown_cloud::store::recover_all(data, &journal)? {
        match r {
            Ok(r) => eprintln!("account {slug}: {r}"),
            Err(e) => eprintln!("warning: account {slug}: recovery failed: {e}"),
        }
    }
    Ok(())
}

fn cloud_command(action: CloudAction) -> Result<ExitCode> {
    match action {
        CloudAction::Account { data, action } => {
            let data = data_dir(data)?;
            recover_accounts(&data)?;
            account_command(&data, action)
        }
        CloudAction::Journal { action } => {
            let data = match &action {
                JournalAction::Tail { data, .. } | JournalAction::Snapshot { data, .. } => {
                    data_dir(data.clone())?
                }
            };
            recover_accounts(&data)?;
            journal_command(&data, action)
        }
        CloudAction::Import(mut args) => {
            if !args.dry_run {
                recover_accounts(&pin_data(&mut args.data)?)?;
            }
            import_command(args)
        }
        CloudAction::ImportAppData(mut args) => {
            if !args.dry_run {
                recover_accounts(&pin_data(&mut args.data)?)?;
            }
            import_app_data_command(args)
        }
        CloudAction::Put(mut args) => {
            recover_accounts(&pin_data(&mut args.data)?)?;
            put_command(args)
        }
        CloudAction::BackupDevice(args) => backup_device_command(args),
        CloudAction::Recycle {
            action: RecycleAction::Prune(mut args),
        } => {
            if !args.dry_run {
                recover_accounts(&pin_data(&mut args.data)?)?;
            }
            recycle_prune_command(args)
        }
        CloudAction::Scan(mut args) => {
            if !args.dry_run {
                recover_accounts(&pin_data(&mut args.data)?)?;
            }
            scan_command(args)
        }
        CloudAction::Render(mut args) => {
            recover_accounts(&pin_data(&mut args.data)?)?;
            render_command(args)
        }
        // No recovery: diagnose writes nothing into the data folder.
        CloudAction::Diagnose(args) => diagnose_command(args),
    }
}

fn diagnose_command(args: DiagnoseArgs) -> Result<ExitCode> {
    let data = data_dir(args.data.clone())?;
    use notedown_cloud::diag::bundle::{DiagnoseOptions, LOCK_WAIT, diagnose};
    let now = time::OffsetDateTime::now_utc();
    let local = notedown_cloud::localtime::local_utc_offset().unwrap_or(time::UtcOffset::UTC);
    let since = notedown_cloud::diag::collect::parse_since(&args.since, now, local)?;
    let out = args.out.unwrap_or_else(|| {
        PathBuf::from(format!(
            "notedown-diag-{:04}{:02}{:02}T{:02}{:02}{:02}Z.zip",
            now.year(),
            now.month() as u8,
            now.day(),
            now.hour(),
            now.minute(),
            now.second()
        ))
    });
    let done = diagnose(&DiagnoseOptions {
        data,
        since,
        device: args.device,
        account: args.account,
        private: args.private,
        out,
        lock_wait: LOCK_WAIT,
        test_keep_key: None,
    })?;
    print!("{}", done.report);
    println!();
    println!(
        "Bundle written: {} ({})",
        done.zip.display(),
        mb(done.bytes)
    );
    Ok(ExitCode::SUCCESS)
}

/// Bytes as megabytes with one decimal, for people.
fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

fn print_scan(scan: &notedown_cloud::import::Scan) {
    use notedown_cloud::import::SkipReason;
    for f in &scan.folders {
        println!(
            "{} -> {}: {}, {}, {}",
            f.source,
            f.target,
            plural(f.files as usize, "file"),
            plural(f.folders as usize, "folder"),
            mb(f.bytes)
        );
    }
    let mut counts: std::collections::BTreeMap<SkipReason, usize> = Default::default();
    for s in &scan.skipped {
        match s.reason {
            SkipReason::SdCard | SkipReason::Unknown | SkipReason::BackupFile => {
                println!("skipped {} ({})", s.path, s.reason.describe());
            }
            SkipReason::Snbak | SkipReason::DotFile | SkipReason::HostJunk => {
                *counts.entry(s.reason).or_default() += 1;
            }
        }
    }
    for (reason, n) in counts {
        println!("skipped {} ({})", plural(n, "file"), reason.describe());
    }
}

/// One progress line of `cloud import --json`.
fn import_progress_json(p: notedown_cloud::import::ImportProgress) -> String {
    use notedown_cloud::import::ImportProgress as P;
    match p {
        P::Recycling { files } => serde_json::json!({"phase": "recycling", "files": files}),
        P::Imported { done, total } => {
            serde_json::json!({"phase": "importing", "done": done, "total": total})
        }
        P::Verifying => serde_json::json!({"phase": "verifying"}),
    }
    .to_string()
}

/// The progress line for people (stderr); `None` where the human output has none.
fn import_progress_text(p: notedown_cloud::import::ImportProgress, slug: &str) -> Option<String> {
    use notedown_cloud::import::{ImportProgress as P, PROGRESS_EVERY};
    match p {
        P::Recycling { files } => Some(format!(
            "moving the {files} files of account {slug} to the recycle bin"
        )),
        // the closing report at a total that is no multiple is for the app only
        P::Imported { done, total } if done > 0 && done % PROGRESS_EVERY == 0 => {
            Some(format!("imported {done} of {total} files"))
        }
        P::Imported { .. } => None,
        P::Verifying => Some("verifying every file against the backup".into()),
    }
}

/// The last line of `cloud import --dry-run --json`.
fn dry_run_json(plan: &notedown_cloud::import::DryRun, replace: bool) -> String {
    let scan = &plan.scan;
    serde_json::json!({"dryRun": {
        "folders": scan.folders.iter().map(|f| serde_json::json!({
            "source": f.source,
            "target": f.target,
            "files": f.files,
            "folders": f.folders,
            "bytes": f.bytes,
        })).collect::<Vec<_>>(),
        "skipped": scan.skipped.iter().map(|s| serde_json::json!({
            "path": s.path,
            "reason": s.reason.describe(),
        })).collect::<Vec<_>>(),
        "errors": scan.errors,
        "filesPresent": plan.files_present,
        "files": scan.files(),
        "bytes": scan.bytes(),
        "needsReplace": plan.files_present > 0 && !replace,
    }})
    .to_string()
}

/// The last line of `cloud import --json`.
fn imported_json(report: &notedown_cloud::import::ImportReport) -> String {
    serde_json::json!({"imported": {
        "files": report.files,
        "bytes": report.bytes,
        "foldersCreated": report.folders_created,
        "replaced": report.replaced,
    }})
    .to_string()
}

/// The last line of `cloud import-app-data --json`.
fn app_data_json(report: &notedown_cloud::appdata::AppImportReport, dry_run: bool) -> String {
    let mut v = serde_json::to_value(report).expect("a report serializes");
    v["dryRun"] = dry_run.into();
    serde_json::json!({"appData": v}).to_string()
}

/// A refusal of `--json` mode as one sentence: the error line is one line for the app.
fn one_sentence(e: impl std::fmt::Display) -> anyhow::Error {
    let text = format!("{e:#}");
    if text.contains("the backup cannot be imported completely") {
        return anyhow::anyhow!("the backup cannot be imported completely");
    }
    let line = text.lines().next().unwrap_or("").trim();
    anyhow::anyhow!(
        "{}",
        line.trim_end_matches(|c: char| c == ':' || c.is_whitespace())
    )
}

/// A JSON line on stdout, flushed at once.
fn print_json_line(line: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn import_command(args: ImportArgs) -> Result<ExitCode> {
    let json = args.json;
    let result = import_command_inner(args);
    if json {
        result.map_err(one_sentence)
    } else {
        result
    }
}

fn import_command_inner(args: ImportArgs) -> Result<ExitCode> {
    let data = data_dir(args.data.clone())?;
    use notedown_cloud::import;
    let options = import::ImportOptions {
        replace: args.replace,
        no_report: args.no_report,
        allow_empty: args.allow_empty,
    };
    let json = args.json;
    exit_with_parent(args.exit_with_parent);
    if args.dry_run {
        let plan = import::dry_run(&data, &args.account, &args.backup, &options)?;
        let empty_refused = plan.scan.files() == 0 && !args.allow_empty;
        if json {
            print_json_line(&dry_run_json(&plan, args.replace));
            if !plan.scan.errors.is_empty() {
                eprintln!("error: the backup cannot be imported completely");
            } else if empty_refused {
                eprintln!("error: the backup holds no files; the import needs --allow-empty");
            } else {
                return Ok(ExitCode::SUCCESS);
            }
            return Ok(ExitCode::from(1));
        }
        println!("dry run, nothing is written");
        print_scan(&plan.scan);
        for e in &plan.scan.errors {
            println!("error: {e}");
        }
        println!(
            "account {} holds {} now{}",
            args.account,
            plural(plan.files_present as usize, "file"),
            if plan.files_present > 0 {
                "; the import needs --replace"
            } else {
                ""
            }
        );
        println!(
            "would import {} ({})",
            plural(plan.scan.files() as usize, "file"),
            mb(plan.scan.bytes())
        );
        if empty_refused {
            println!("error: the backup holds no files; the import needs --allow-empty");
        }
        return Ok(if plan.scan.errors.is_empty() && !empty_refused {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        });
    }
    let journal = notedown_cloud::journal::Journal::open(&data)?;
    let slug = args.account.clone();
    let result = import::import(
        &data,
        &args.account,
        &args.backup,
        &options,
        &journal,
        &mut |p| {
            if json {
                print_json_line(&import_progress_json(p));
            } else if let Some(line) = import_progress_text(p, &slug) {
                eprintln!("{line}");
            }
        },
    );
    let report = if json {
        result.map_err(one_sentence)?
    } else {
        result?
    };
    if json {
        print_json_line(&imported_json(&report));
        return Ok(ExitCode::SUCCESS);
    }
    print_scan(&report.scan);
    if report.replaced > 0 {
        println!(
            "moved {} of the old tree to the recycle bin",
            plural(report.replaced as usize, "file")
        );
    }
    println!(
        "imported {} ({}), protect gate open for {}",
        plural(report.files as usize, "file"),
        mb(report.bytes),
        args.account
    );
    Ok(ExitCode::SUCCESS)
}

fn import_app_data_command(args: ImportAppDataArgs) -> Result<ExitCode> {
    let json = args.json;
    let result = import_app_data_command_inner(args);
    if json {
        result.map_err(one_sentence)
    } else {
        result
    }
}

fn import_app_data_command_inner(args: ImportAppDataArgs) -> Result<ExitCode> {
    let data = data_dir(args.data.clone())?;
    use notedown_cloud::appdata::{AppImportOptions, dry_run_app_data, import_app_data};
    let json = args.json;
    exit_with_parent(args.exit_with_parent);
    let result = if args.dry_run {
        // No lock and no journal: a dry run writes nothing, not even a folder.
        dry_run_app_data(&data, &args.account, args.snbak.as_deref(), args.replace)
    } else {
        let journal = notedown_cloud::journal::Journal::open(&data)?;
        import_app_data(
            &data,
            &args.account,
            args.snbak.as_deref(),
            &AppImportOptions {
                dry_run: false,
                replace: args.replace,
            },
            &journal,
        )
    };
    let report = if json {
        result.map_err(one_sentence)?
    } else {
        result?
    };
    if json {
        print_json_line(&app_data_json(&report, args.dry_run));
        return Ok(ExitCode::SUCCESS);
    }
    if args.dry_run {
        println!("dry run, nothing is written");
        for line in &report.mapping {
            println!("{line}");
        }
    }
    for line in &report.left_to_device {
        println!("no cloud id yet, left to the device: {line}");
    }
    for line in &report.without_group {
        println!("its group is not imported: {line}");
    }
    for name in &report.skipped {
        println!("skipped {name} (no imported digest names it)");
    }
    if report.replaced > 0 {
        println!(
            "{} {} of the account {}",
            if args.dry_run {
                "would clear"
            } else {
                "cleared"
            },
            plural(report.replaced as usize, "digest record"),
            args.account
        );
    }
    if !report.left_to_device.is_empty() {
        let n = report.left_to_device.len();
        println!(
            "{} left to the device (the device adds {} itself at its first sync)",
            plural(n, "record"),
            if n == 1 { "it" } else { "them" }
        );
    }
    println!(
        "{} {}, {}, {} into account {}",
        if args.dry_run {
            "would import"
        } else {
            "imported"
        },
        plural(report.digests as usize, "digest"),
        plural(report.groups as usize, "group"),
        plural(report.marks as usize, "handwritten comment"),
        args.account
    );
    Ok(ExitCode::SUCCESS)
}

fn put_command(args: PutArgs) -> Result<ExitCode> {
    let data = data_dir(args.data.clone())?;
    use notedown_cloud::put::{PutOutcome, put};
    let journal = notedown_cloud::journal::Journal::open(&data)?;
    let r = put(
        &data,
        &args.account,
        &journal,
        &args.file,
        &args.to,
        args.name.as_deref(),
    )
    .map_err(|e| held_error(&data, &args.account, e))?;
    let what = match r.outcome {
        PutOutcome::Created => "created",
        PutOutcome::Replaced => "replaced, the old version is in the recycle bin",
        PutOutcome::Unchanged => "unchanged, the same bytes are there already",
    };
    println!("{} id {} ({what})", r.path, r.id);
    Ok(ExitCode::SUCCESS)
}

/// `ms` since the epoch as local `YYYY-MM-DD HH:MM`.
fn local_time(ms: u64, offset: time::UtcOffset) -> String {
    match time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000) {
        Ok(t) => {
            let t = t.to_offset(offset);
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}",
                t.year(),
                u8::from(t.month()),
                t.day(),
                t.hour(),
                t.minute()
            )
        }
        Err(_) => format!("{ms} ms"),
    }
}

fn recycle_prune_command(args: RecyclePruneArgs) -> Result<ExitCode> {
    let data = data_dir(args.data.clone())?;
    use notedown_cloud::journal::{Journal, Origin};
    use notedown_cloud::store::{RecycleReason, Store};
    // A dry run leaves the registry log as it is.
    let store = if args.dry_run {
        Store::open_no_compact(&data, &args.account)?
    } else {
        Store::open(&data, &args.account)?
    };
    // Never write server.toml here, and a dry run creates nothing at all.
    let config = notedown_cloud::Config::load_or_default(&data)?;
    let journal = if args.dry_run {
        Journal::open_no_create(&data)
    } else {
        Journal::open(&data)?
    };
    let offset = notedown_cloud::localtime::local_utc_offset().unwrap_or_else(|| {
        eprintln!("warning: can't read the local UTC offset; days are counted in UTC");
        time::UtcOffset::UTC
    });
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let report = store.prune_recycle(
        &journal,
        &config.retention(),
        now_ms,
        offset,
        Origin::Cli,
        args.dry_run,
    )?;
    let verb = if args.dry_run {
        "would remove"
    } else {
        "removed"
    };
    for e in &report.removed {
        let reason = match e.reason {
            RecycleReason::Overwritten => "overwritten",
            RecycleReason::Deleted => "deleted",
        };
        println!(
            "{verb} {}  {}  {reason}",
            e.path,
            local_time(e.deleted_at, offset)
        );
    }
    println!(
        "{verb} {} ({}), kept {}",
        plural(report.removed.len(), "old version"),
        mb(report.removed_bytes),
        report.kept
    );
    if report.unreadable_kept > 0 {
        println!(
            "kept {} that can't be read",
            plural(report.unreadable_kept, "index line")
        );
    }
    // A dry run only reports: it fails on nothing it found.
    let label = if args.dry_run { "warning" } else { "error" };
    for f in &report.failed {
        eprintln!("{label}: recycle/{}: {}", f.location, f.message);
    }
    Ok(if args.dry_run || report.failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// The counts of a scan as "3 new, 1 changed, 1 moved, 2 deleted"; parts that are 0 are left out.
fn scan_counts(r: &notedown_cloud::store::ScanReport) -> String {
    let parts: Vec<String> = [
        (r.created.len(), "new"),
        (r.modified.len(), "changed"),
        (r.moved.len(), "moved"),
        (r.deleted.len(), "deleted"),
    ]
    .iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, w)| format!("{n} {w}"))
    .collect();
    parts.join(", ")
}

/// Scan one account and print the result. Returns true when the scan held.
fn scan_account(
    data: &Path,
    slug: &str,
    args: &ScanArgs,
    min_files: u64,
    journal: &notedown_cloud::journal::Journal,
) -> Result<bool> {
    use notedown_cloud::store::{ScanOptions, Store};
    // A dry run leaves the registry log of earlier writes as it is.
    let store = if args.dry_run {
        Store::open_no_compact(data, slug)?
    } else {
        Store::open(data, slug)?
    };
    let guard = if args.dry_run {
        // A dry run writes nothing, so it can't finish an unfinished change either.
        store.lock_unless_pending()?.ok_or_else(|| {
            anyhow!(
                "account {slug} has an unfinished change. Run the scan without --dry-run, or \
                 start the server, to finish it first"
            )
        })?
    } else {
        store.lock_recovered(journal)?
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let mut opts = ScanOptions::all(min_files);
    opts.dry_run = args.dry_run;
    opts.accept = args.accept;
    let report = store.scan(&guard, journal, &opts, now_ms)?;
    if args.dry_run || report.held.is_some() {
        for p in &report.created {
            println!("would add {p}");
        }
        for p in &report.modified {
            println!("would update {p}");
        }
        for (from, to) in &report.moved {
            println!("would move {from} to {to}");
        }
        for p in &report.deleted {
            println!("would delete {p}");
        }
    }
    if let Some(hold) = &report.held {
        println!(
            "account {slug}: would delete {}, and only {} would be left (the minimum is {}).",
            plural(hold.gone, "file"),
            hold.left,
            hold.min
        );
        let accept = notedown_cloud::server::accept_command(data, slug);
        if args.dry_run {
            println!("Device sync would be put on hold. If that's on purpose, run: {accept}");
        } else {
            println!("Device sync is on hold. If that's on purpose, run: {accept}");
        }
        return Ok(true);
    }
    let counts = scan_counts(&report);
    let verb = if args.dry_run {
        "would journal"
    } else {
        "journaled"
    };
    if counts.is_empty() {
        println!("account {slug}: no changes");
    } else {
        println!("account {slug}: {verb} {counts}");
    }
    if report.released {
        let text = if args.dry_run {
            "would lift the hold"
        } else {
            "the hold is gone"
        };
        println!("account {slug}: {text}");
    }
    Ok(false)
}

/// One render pass for one account, printed. Returns true when a note failed.
fn render_one_account(data: &Path, slug: &str, root: &Path, all: bool) -> Result<bool> {
    use notedown_cloud::render::{PassOptions, render_account};
    let store = notedown_cloud::store::Store::open_announced(data, slug)?;
    let opts = PassOptions {
        all,
        wait_for_lock: true,
    };
    let r = render_account(&store, root, &opts)?;
    for p in &r.written {
        println!("written {p}");
    }
    for (from, to) in &r.moved {
        println!("moved {from} -> {to}");
    }
    for p in &r.deleted {
        println!("deleted {p}");
    }
    for (p, e) in &r.failed {
        println!("failed {p}: {e}");
    }
    for p in &r.kept_foreign {
        println!("kept {p} (not written by notedown)");
    }
    println!(
        "{slug}: written: {}, moved: {}, deleted: {}, failed: {}",
        r.written.len(),
        r.moved.len(),
        r.deleted.len(),
        r.failed.len()
    );
    Ok(!r.failed.is_empty())
}

fn render_command(args: CloudRenderArgs) -> Result<ExitCode> {
    let data = data_dir(args.data.clone())?;
    if !data.is_dir() {
        return Err(anyhow!("data folder {} does not exist", data.display()));
    }
    // Never write server.toml here.
    let config = notedown_cloud::Config::load_or_default(&data)?;
    let root = config
        .render_root(&data)
        .ok_or_else(|| anyhow!("render_dir is empty in server.toml: rendering is off"))?;
    let slugs = match &args.account {
        Some(slug) => vec![slug.clone()],
        None => notedown_cloud::auth::Accounts::new(&data)
            .list()?
            .into_iter()
            .map(|a| a.slug)
            .collect(),
    };
    let mut failed = false;
    for slug in &slugs {
        // With --account the error ends the command; otherwise the other accounts still run.
        match render_one_account(&data, slug, &root, args.all) {
            Ok(f) => failed |= f,
            Err(e) if args.account.is_some() => return Err(e),
            Err(e) => {
                eprintln!("error: account {slug}: {e}");
                failed = true;
            }
        }
    }
    Ok(if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// `e`, with the way out when it is a scan hold of account `slug`.
fn held_error(data: &Path, slug: &str, e: notedown_cloud::Error) -> anyhow::Error {
    match e {
        notedown_cloud::Error::ScanHold(_) => anyhow!(
            "account {slug} is on hold: files are gone from the data folder. Put them back, or \
             if that's on purpose, run: {}",
            notedown_cloud::server::accept_command(data, slug)
        ),
        e => e.into(),
    }
}

fn scan_command(args: ScanArgs) -> Result<ExitCode> {
    let data = data_dir(args.data.clone())?;
    use notedown_cloud::journal::Journal;
    if !data.is_dir() {
        return Err(anyhow!("data folder {} does not exist", data.display()));
    }
    // Never write server.toml here, and a dry run creates nothing at all.
    let config = notedown_cloud::Config::load_or_default(&data)?;
    let journal = if args.dry_run {
        Journal::open_no_create(&data)
    } else {
        Journal::open(&data)?
    };
    let slugs = match &args.account {
        Some(slug) => vec![slug.clone()],
        None => notedown_cloud::auth::Accounts::new(&data)
            .list()?
            .into_iter()
            .map(|a| a.slug)
            .collect(),
    };
    let mut failed = false;
    for slug in &slugs {
        // With --account the error ends the command; otherwise the other accounts still run.
        match scan_account(&data, slug, &args, config.scan_min_files, &journal) {
            Ok(held) => failed |= held,
            Err(e) if args.account.is_some() => return Err(e),
            Err(e) => {
                eprintln!("error: account {slug}: {e}");
                failed = true;
            }
        }
    }
    Ok(if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// One progress line of `cloud backup-device --json`.
fn backup_progress_json(files: u64, bytes: u64, path: &str) -> String {
    // written by hand: the keys keep this order
    let path = serde_json::Value::from(path);
    format!(r#"{{"files":{files},"bytes":{bytes},"path":{path}}}"#)
}

/// The last line of `cloud backup-device --json`: the content of backup.json.
fn backup_report_json(report: &notedown_cloud::device_backup::BackupReport) -> String {
    // the report serializes in the field order of backup.json
    let report = serde_json::to_string(report).expect("a report serializes");
    format!(r#"{{"report":{report}}}"#)
}

fn backup_device_command(args: BackupDeviceArgs) -> Result<ExitCode> {
    use notedown_cloud::device_backup::{BackupOptions, backup};
    use notedown_cloud::localtime::{local_utc_offset, parse_utc_offset};
    let utc_offset = match &args.utc_offset {
        Some(t) => parse_utc_offset(t)
            .ok_or_else(|| anyhow!("--utc-offset {t:?}: use a form like +02:00 or -05:00"))?,
        None => local_utc_offset().unwrap_or_else(|| {
            eprintln!("warning: cannot read the local UTC offset; file dates are read as UTC");
            time::UtcOffset::UTC
        }),
    };
    let options = BackupOptions {
        include_sd: args.include_sd,
        resume: args.resume,
        utc_offset,
    };
    // SIGTERM ends the run as it is; the .part files stay and --resume continues.
    exit_with_parent(args.exit_with_parent);
    let json = args.json;
    let mut last: Option<std::time::Instant> = None;
    // the newest progress not yet printed (json mode), printed at the end at the latest
    let mut pending: Option<(u64, u64, String)> = None;
    let printed = |files: u64, bytes: u64, path: &str| {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{}", backup_progress_json(files, bytes, path));
        let _ = out.flush();
    };
    let report = backup(&args.url, &args.dir, &options, &mut |event| {
        use notedown_cloud::device_backup::BackupProgress as P;
        match event {
            P::Downloaded { files, bytes, path } if json => {
                if last.is_none_or(|t| t.elapsed() >= Duration::from_millis(250)) {
                    last = Some(std::time::Instant::now());
                    pending = None;
                    printed(files, bytes, path);
                } else {
                    pending = Some((files, bytes, path.to_string()));
                }
            }
            P::Downloaded { files, bytes, .. } => {
                if files % 100 == 0 {
                    eprintln!("downloaded {files} files ({bytes} bytes so far)");
                }
            }
            // json mode: the report has these, and stderr holds one error line at most
            _ if json => {}
            P::Error { path, message } => eprintln!("error: {path}: {message}"),
            P::Warning { path, message } => eprintln!("warning: {path}: {message}"),
            P::SkippedSd { name } => eprintln!("skipped {name} (the SD card)"),
            P::Stale { path } if path.ends_with('/') => {
                eprintln!("stale: {path}: the device no longer lists this folder")
            }
            P::Stale { path } => {
                eprintln!("stale: {path}: the device no longer lists this file")
            }
        }
    })?;
    if json {
        if let Some((files, bytes, path)) = pending {
            printed(files, bytes, &path);
        }
        println!("{}", backup_report_json(&report));
        if !report.stale.is_empty() {
            eprintln!(
                "error: the device changed since the earlier run; make a fresh backup into a new folder"
            );
            return Ok(ExitCode::from(1));
        }
        if !report.errors.is_empty() {
            eprintln!(
                "error: {} could not be backed up",
                plural(report.errors.len(), "file")
            );
            return Ok(ExitCode::from(1));
        }
        return Ok(ExitCode::SUCCESS);
    }
    for (folder, t) in &report.folders {
        println!(
            "{folder}: {}, {}",
            plural(t.files as usize, "file"),
            mb(t.bytes)
        );
    }
    if report.date_warnings > 0 {
        println!(
            "{} could not be read (see backup.json)",
            plural(report.date_warnings as usize, "file date")
        );
    }
    if report.skipped_existing > 0 {
        println!(
            "kept {} from the earlier run",
            plural(report.skipped_existing as usize, "file")
        );
    }
    let report_file = args.dir.join(notedown_cloud::import::BACKUP_FILE);
    if !report.stale.is_empty() {
        eprintln!(
            "{} the device no longer lists (deleted or renamed since the earlier run), see {}. \
             The import refuses this backup. Make the final backup a fresh run into a new, \
             empty folder.",
            plural(report.stale.len(), "local file"),
            report_file.display()
        );
        return Ok(ExitCode::from(1));
    }
    if report.errors.is_empty() {
        println!(
            "backed up {} ({}) from {} into {}",
            plural(report.files as usize, "file"),
            mb(report.bytes),
            args.url,
            args.dir.display()
        );
        Ok(ExitCode::SUCCESS)
    } else {
        println!(
            "backed up {} ({}) with {}, see {}",
            plural(report.files as usize, "file"),
            mb(report.bytes),
            plural(report.errors.len(), "error"),
            report_file.display()
        );
        Ok(ExitCode::from(1))
    }
}

/// Print the events from `*next` on and move `*next` past them. Returns false when stdout is
/// closed (a pipe into `head`), which ends the command cleanly.
fn print_events(journal: &notedown_cloud::journal::Journal, next: &mut u64) -> Result<bool> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for event in journal.read_from(*next)? {
        let event = event?;
        let written = writeln!(out, "{}", event.to_line());
        match written {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => return Ok(false),
            Err(e) => return Err(e.into()),
        }
        *next = event.seq + 1;
    }
    match out.flush() {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn journal_command(data: &Path, action: JournalAction) -> Result<ExitCode> {
    use notedown_cloud::journal::Journal;
    if !data.is_dir() {
        return Err(anyhow!("data folder {} does not exist", data.display()));
    }
    let journal = Journal::open(data)?;
    match action {
        JournalAction::Tail { from, follow, .. } => {
            let mut next = from.unwrap_or(0);
            if !print_events(&journal, &mut next)? || !follow {
                return Ok(ExitCode::SUCCESS);
            }
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let mut stop = std::pin::pin!(tokio::signal::ctrl_c());
                loop {
                    tokio::select! {
                        _ = &mut stop => return Ok::<(), anyhow::Error>(()),
                        () = tokio::time::sleep(Duration::from_secs(1)) => {
                            if !print_events(&journal, &mut next)? {
                                return Ok(());
                            }
                        }
                    }
                }
            })?;
        }
        JournalAction::Snapshot { account, .. } => {
            use notedown_cloud::store::{ScanOptions, Store};
            let min_files = notedown_cloud::Config::load_or_default(data)?.scan_min_files;
            let store = Store::open(data, &account)?;
            let guard = store.lock_recovered(&journal)?;
            // The changes made by hand first: the snapshot must not give them ids without
            // their events.
            let report = store.scan(
                &guard,
                &journal,
                &ScanOptions::all(min_files),
                journal.now_ms(),
            )?;
            if report.held.is_some() {
                return Err(held_error(
                    data,
                    &account,
                    notedown_cloud::Error::ScanHold(account.clone()),
                ));
            }
            let seqs = store.snapshot(&guard, &journal)?;
            match (seqs.first(), seqs.last()) {
                (Some(a), Some(b)) => println!(
                    "journaled {} for account {account} (seq {a} to {b})",
                    plural(seqs.len(), "event")
                ),
                _ => println!("account {account} has no files, folders, digests or to-dos"),
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn account_command(data: &Path, action: AccountAction) -> Result<ExitCode> {
    use notedown_cloud::auth::Accounts;
    let accounts = Accounts::new(data);
    match action {
        AccountAction::Add {
            slug,
            email,
            password_stdin,
        } => {
            // Validate before asking for a password.
            notedown_cloud::auth::validate_slug(&slug)?;
            if accounts.get(&slug)?.is_some() {
                return Err(anyhow!("account {slug} already exists"));
            }
            let password = read_password(password_stdin, true)?;
            let a = accounts.create(&slug, &email, &password)?;
            println!("created account {} (user id {})", a.slug, a.user_id);
        }
        AccountAction::Passwd {
            slug,
            password_stdin,
        } => {
            if accounts.get(&slug)?.is_none() {
                return Err(anyhow!("account {slug} does not exist"));
            }
            let password = read_password(password_stdin, true)?;
            let revoked = accounts.set_password(&slug, &password)?;
            println!("password of {slug} changed");
            if revoked > 0 {
                println!("revoked {revoked} REST tokens");
            }
        }
        AccountAction::List => {
            for a in accounts.list()? {
                println!(
                    "{}\t{}\t{}\t{}",
                    a.slug,
                    a.email,
                    a.user_id,
                    if a.locked { "locked" } else { "active" }
                );
            }
        }
        AccountAction::Remove { slug, yes } => {
            if !yes {
                return Err(anyhow!(
                    "this deletes account {slug} with all its files and its note text; add --yes to confirm"
                ));
            }
            if accounts.get(&slug)?.is_none() {
                return Err(anyhow!("account {slug} does not exist"));
            }
            let journal = notedown_cloud::journal::Journal::open(data)?;
            accounts.remove(&slug, &journal)?;
            println!("removed account {slug}");
        }
        AccountAction::Tokens { action } => tokens_command(data, action)?,
    }
    Ok(ExitCode::SUCCESS)
}

fn tokens_command(data: &Path, action: TokensAction) -> Result<()> {
    use notedown_cloud::auth::{list_tokens, revoke_all_tokens, revoke_token};
    match action {
        TokensAction::List { slug } => {
            let tokens = list_tokens(data, &slug)?;
            if tokens.is_empty() {
                println!("no tokens");
            }
            for t in tokens {
                println!(
                    "{}  {}  {}  created {}",
                    t.id,
                    t.scope,
                    t.name,
                    rfc3339_ms(t.created_ms)
                );
            }
        }
        TokensAction::Revoke {
            slug, id: Some(id), ..
        } => {
            if !revoke_token(data, &slug, &id)? {
                return Err(anyhow!("no token {id} for {slug}"));
            }
            println!("revoked {id}");
        }
        TokensAction::Revoke { slug, id: None, .. } => {
            let n = revoke_all_tokens(data, &slug)?;
            println!("revoked {n} tokens");
        }
    }
    Ok(())
}

fn rfc3339_ms(ms: u64) -> String {
    use time::format_description::well_known::Rfc3339;
    i64::try_from(ms / 1000)
        .ok()
        .and_then(|s| time::OffsetDateTime::from_unix_timestamp(s).ok())
        .and_then(|t| t.format(&Rfc3339).ok())
        .unwrap_or_else(|| ms.to_string())
}

/// The URLs a device can use: `http://<ip>:<port>` for each listen address. A wildcard address
/// expands to every non-loopback IPv4 address of this host (`host_ips`).
fn device_urls(listen: &[std::net::SocketAddr], host_ips: &[std::net::Ipv4Addr]) -> Vec<String> {
    let mut urls = Vec::new();
    for addr in listen {
        match addr.ip() {
            std::net::IpAddr::V4(ip) if ip.is_unspecified() => {
                urls.extend(
                    host_ips
                        .iter()
                        .map(|h| format!("http://{h}:{}", addr.port())),
                );
            }
            std::net::IpAddr::V4(ip) if !ip.is_loopback() => {
                urls.push(format!("http://{ip}:{}", addr.port()));
            }
            _ => {}
        }
    }
    urls
}

/// The base URLs of the REST API: the device URLs, else (loopback only) the bound addresses
/// that a client can use. An unspecified address (`0.0.0.0`, `[::]`) is never one.
fn rest_bases(bound: &[std::net::SocketAddr], device_urls: &[String]) -> Vec<String> {
    if !device_urls.is_empty() {
        return device_urls.to_vec();
    }
    bound
        .iter()
        .filter(|a| !a.ip().is_unspecified())
        .map(|a| format!("http://{a}"))
        .collect()
}

fn host_ipv4s() -> Vec<std::net::Ipv4Addr> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|i| match i.ip() {
            std::net::IpAddr::V4(ip) if !ip.is_loopback() => Some(ip),
            _ => None,
        })
        .collect()
}

fn serve_command(args: ServeArgs) -> Result<ExitCode> {
    let data = data_dir(args.data.clone())?;
    use notedown_cloud::Config;
    use notedown_cloud::server::{AppState, bind, serve_until_with};
    let (mut config, config_warnings) = Config::load_with_warnings(&data)?;
    // The flag counts for this run only; server.toml is not rewritten.
    let capture_from_file = config.capture && !args.capture;
    config.capture |= args.capture;
    if args.refuse_sync {
        config.sync = notedown_cloud::SyncMode::Refuse;
    }
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let listeners = bind(&config).await?;
        let state = AppState::open(config.clone(), &data)?;
        println!("notedown cloud server");
        for l in &listeners {
            println!("listening on: {}", l.local_addr()?);
        }
        let bound: Vec<_> = listeners
            .iter()
            .filter_map(|l| l.local_addr().ok())
            .collect();
        let urls = device_urls(&bound, &host_ipv4s());
        if urls.is_empty() {
            println!("device URL: none found, use the IP address of this host");
        }
        for url in &urls {
            println!("device URL: {url}");
        }
        let bases = rest_bases(&bound, &urls);
        if bases.is_empty() {
            println!("REST API: none found, use the IP address of this host");
        }
        for base in &bases {
            println!(
                "REST API: {base}{} (connect a pipeline with notedown connect {base})",
                notedown_cloud::rest::PREFIX
            );
        }
        if let Some(url) = &config.external_base_url {
            println!("external base URL: {url}");
        }
        println!("data folder: {}", data.display());
        println!("log folder: {}", state.log.logs_dir().display());
        if config.capture {
            println!("capture dumps: {}/capture", data.display());
        }
        if capture_from_file {
            eprintln!(
                "warning: capture = true is set in server.toml. Capture is for one capture \
                 session only, the dumps have no size cap. Set capture = false and use --capture."
            );
        }
        for warning in &config_warnings {
            eprintln!("warning: {warning}");
        }
        match config.sync {
            notedown_cloud::SyncMode::Refuse => {
                println!("sync mode: refuse (capture only, no files are exchanged)");
            }
            notedown_cloud::SyncMode::Accept => {
                println!(
                    "sync mode: accept (protect: {})",
                    if config.protect { "on" } else { "off" }
                );
                println!(
                    "push mode: {}",
                    match config.push {
                        notedown_cloud::PushMode::Detail => {
                            "detail (each changed file and folder, after the device's next heartbeat)"
                        }
                        notedown_cloud::PushMode::Startsync => "startsync",
                        notedown_cloud::PushMode::Off => "off (devices sync on their own schedule)",
                    }
                );
            }
        }
        match config.render_root(&data) {
            Some(root) => println!("render: {}", root.display()),
            None => println!("render: off"),
        }
        if config.mcp.is_some() {
            println!(
                "mcp: [mcp] in server.toml is ignored; MCP runs on the pipeline machine over stdio (notedown mcp)"
            );
        }
        println!("watch: {}", if config.watch { "on" } else { "off" });
        if config.recycle_prune {
            println!(
                "recycle bin: keeps {} newest, {} daily, {} monthly, deleted files {} days",
                config.recycle_keep_latest,
                config.recycle_keep_daily,
                config.recycle_keep_monthly,
                config.recycle_keep_deleted_days
            );
        } else {
            println!("recycle bin: never pruned");
        }
        if config.scan_interval > 0 {
            println!(
                "scan of the data folder: every {} s, at sync start and before listings",
                config.scan_interval
            );
        } else {
            println!("scan of the data folder: at sync start and before listings (no timer)");
        }
        match config.scan_min_files {
            0 => println!("scan hold: off"),
            1 => println!("scan hold: when the account would be left without files"),
            n => println!("scan hold: when fewer than {n} files would be left"),
        }
        // The handlers are in place before the child starts, so a stop signal always ends
        // the server and with it the child.
        let shutdown = Shutdown::new()?;
        exit_with_parent(args.exit_with_parent);
        #[cfg(feature = "ocr")]
        let child: PipelineChild = std::sync::Arc::new(std::sync::Mutex::new(None));
        #[cfg(feature = "ocr")]
        let guard = PipelineGuard(child.clone());
        #[cfg(feature = "ocr")]
        if args.pipeline {
            let started = start_pipeline(&data)?;
            println!("pipeline: started notedown pipeline (pid {})", started.id());
            *child.lock().unwrap_or_else(|e| e.into_inner()) = Some(started);
            tokio::spawn(watch_pipeline(child.clone()));
        }
        println!("press Ctrl-C to stop");
        std::io::stdout().flush()?;
        let served = serve_until_with(
            listeners,
            state.clone(),
            notedown_cloud::rest::router(state.clone()),
            shutdown.wait(),
        )
        .await;
        #[cfg(feature = "ocr")]
        drop(guard);
        served?;
        println!("stopped");
        let (dropped, failed) = (state.log.dropped_lines(), state.log.capture_failures());
        if dropped > 0 {
            eprintln!("warning: {dropped} log lines could not be written");
        }
        if failed > 0 {
            eprintln!("warning: {failed} capture dumps could not be written");
        }
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(ExitCode::SUCCESS)
}

/// Ctrl-C and, on Unix, SIGTERM. The handlers are in place once [`Shutdown::new`] returns
/// (call it inside a tokio runtime).
struct Shutdown {
    #[cfg(unix)]
    signals: (tokio::signal::unix::Signal, tokio::signal::unix::Signal),
}

impl Shutdown {
    fn new() -> Result<Shutdown> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Shutdown {
                signals: (
                    signal(SignalKind::interrupt())?,
                    signal(SignalKind::terminate())?,
                ),
            })
        }
        #[cfg(not(unix))]
        Ok(Shutdown {})
    }

    async fn wait(self) {
        #[cfg(unix)]
        {
            let (mut int, mut term) = self.signals;
            tokio::select! {
                _ = int.recv() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// With `--exit-with-parent <pid>`: a thread checks once per second whether `pid` is still the
/// parent of this process. When it is not, it sends this process SIGTERM, so it stops the same
/// way as on a stop signal. macOS has no parent-death signal; this is how a child of the Mac app
/// ends when the app dies without a clean Quit. Call it once the stop handlers are in place.
fn exit_with_parent(parent: Option<u32>) {
    #[cfg(unix)]
    {
        let Some(parent) = parent else {
            return;
        };
        std::thread::spawn(move || {
            while std::os::unix::process::parent_id() == parent {
                std::thread::sleep(Duration::from_secs(1));
            }
            let sent = std::process::Command::new("kill")
                .args(["-TERM", &std::process::id().to_string()])
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if !sent {
                eprintln!("the parent process {parent} is gone and SIGTERM failed, stopping now");
                std::process::exit(1);
            }
        });
    }
    #[cfg(not(unix))]
    let _ = parent;
}

#[cfg(feature = "ocr")]
/// `notedown pipeline --data <data>` as a child process of the server.
fn start_pipeline(data: &Path) -> Result<std::process::Child> {
    let exe = std::env::current_exe().context("cannot find the notedown program")?;
    std::process::Command::new(exe)
        .arg("pipeline")
        .arg("--data")
        .arg(data)
        .stdin(std::process::Stdio::null())
        .spawn()
        .context("cannot start notedown pipeline")
}

#[cfg(feature = "ocr")]
type PipelineChild = std::sync::Arc<std::sync::Mutex<Option<std::process::Child>>>;

#[cfg(feature = "ocr")]
/// Says once when the pipeline child ends by itself; the server keeps running.
async fn watch_pipeline(child: PipelineChild) {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let mut guard = child.lock().unwrap_or_else(|e| e.into_inner());
        let Some(c) = guard.as_mut() else {
            return;
        };
        match c.try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => {
                println!("pipeline: notedown pipeline ended ({status}), the server keeps running");
                *guard = None;
                return;
            }
            Err(e) => {
                println!("pipeline: cannot watch notedown pipeline: {e}");
                return;
            }
        }
    }
}

#[cfg(feature = "ocr")]
/// How long the pipeline child gets to stop after SIGTERM before it is killed.
const PIPELINE_STOP_WAIT: Duration = Duration::from_secs(5);

#[cfg(feature = "ocr")]
/// The pipeline child of `serve --pipeline`. Dropping the guard stops the child, also on an
/// error or a panic of the server.
struct PipelineGuard(PipelineChild);

#[cfg(feature = "ocr")]
impl Drop for PipelineGuard {
    fn drop(&mut self) {
        stop_pipeline(&self.0);
    }
}

#[cfg(feature = "ocr")]
/// Stops the pipeline child, if it still runs: SIGTERM (it ends between two pages), then a
/// kill after [`PIPELINE_STOP_WAIT`].
fn stop_pipeline(child: &PipelineChild) {
    let mut guard = child.lock().unwrap_or_else(|e| e.into_inner());
    let Some(mut c) = guard.take() else {
        return;
    };
    if let Ok(Some(_)) = c.try_wait() {
        return;
    }
    #[cfg(unix)]
    {
        let sent = std::process::Command::new("kill")
            .args(["-TERM", &c.id().to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if sent {
            let end = Instant::now() + PIPELINE_STOP_WAIT;
            while Instant::now() < end {
                if !matches!(c.try_wait(), Ok(None)) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    let _ = c.kill();
    let _ = c.wait();
}

#[cfg(feature = "ocr")]
/// Sets the flag on Ctrl-C or SIGTERM. The handlers are in place when this returns.
fn stop_flag() -> Result<std::sync::Arc<std::sync::atomic::AtomicBool>> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let shutdown = {
        let _guard = runtime.enter();
        Shutdown::new()?
    };
    std::thread::spawn(move || {
        runtime.block_on(shutdown.wait());
        flag.store(true, Ordering::SeqCst);
    });
    Ok(stop)
}

#[cfg(feature = "ocr")]
fn pipeline_command(args: PipelineArgs) -> Result<ExitCode> {
    use notedown_pipeline::worker::{EngineLoader, Worker};
    if let Some(action) = args.action {
        return pipeline_action(action);
    }
    let data = data_dir(args.data)?;
    if !data.is_dir() {
        return Err(anyhow!("data folder {} does not exist", data.display()));
    }
    // Without <data>/pipeline.toml the server's data is this folder; with it, the folder named
    // there, and this one keeps only the pipeline's own data.
    let mut flags = Vec::new();
    for text in &args.markdown_dir {
        let (slug, dir) = notedown_pipeline::setup::parse_markdown_flag(text);
        let dir = std::path::absolute(&dir).with_context(|| format!("--markdown-dir {text}"))?;
        flags.push((slug, dir));
    }
    let setup = notedown_pipeline::setup::Setup::load(&data)?
        .with_markdown_dirs(flags)?
        .with_model_idle(args.model_idle.as_deref())?;
    // A folder without an account needs exactly one served account. Checked now when the
    // account list can be read; the worker checks it again on every pass.
    if setup.markdown_only.is_some()
        && let Ok(served) = setup.served()
    {
        let slugs: Vec<String> = served.into_iter().map(|a| a.slug).collect();
        setup.check_markdown(&slugs)?;
    }
    // The model is loaded when a page is due, as `notedown convert` loads it.
    let loader: EngineLoader = Box::new(|| {
        let paths = model::resolve(None, None)?;
        eprintln!("loading model {} ...", paths.label);
        let engine = VisionEngine::load(&paths, OcrOptions::default())?;
        Ok(Box::new(engine) as Box<dyn OcrEngine>)
    });
    let mut worker = Worker::with_loader(&setup, loader);
    if args.once || args.reocr {
        let done = if args.reocr {
            worker.reocr(args.note.as_deref())
        } else {
            worker.run_once(true)
        };
        let p = match done {
            Ok(p) => p,
            // The worker said so already, in one line.
            Err(notedown_pipeline::Error::ModelMissing) => return Ok(ExitCode::from(1)),
            Err(e) => return Err(e.into()),
        };
        println!(
            "{} read, {} written, {} failed",
            plural(p.pages_read, "page"),
            plural(p.versions, "version"),
            p.failed
        );
        return Ok(if p.failed > 0 {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        });
    }
    let stop = stop_flag()?;
    exit_with_parent(args.exit_with_parent);
    // Said only once the writer lock is held: a refused second writer prints only the refusal.
    let started = || {
        eprintln!(
            "notedown pipeline: running for {}, press Ctrl-C to stop",
            data.display()
        )
    };
    worker.run_with_start(started, || stop.load(std::sync::atomic::Ordering::SeqCst))?;
    eprintln!("notedown pipeline: stopped");
    Ok(ExitCode::SUCCESS)
}

#[cfg(feature = "ocr")]
fn pipeline_action(action: PipelineAction) -> Result<ExitCode> {
    use notedown_pipeline::delta::{self, MAX_CHARS, Start};
    use notedown_pipeline::format::to_json;
    use notedown_pipeline::mcp::resolve_account;
    use notedown_pipeline::store::TextStore;
    match action {
        PipelineAction::Consumers {
            data,
            account,
            remove,
        } => {
            let data = data_dir(data)?;
            if let Some(name) = remove {
                let slug = resolve_account(&data, account.as_deref())?;
                let store = TextStore::open(&data, &slug)?;
                if !store.remove_consumer(&name)? {
                    return Err(anyhow!("no bookmark {name} in account {slug}"));
                }
                println!("removed the bookmark of {name} in account {slug}");
                return Ok(ExitCode::SUCCESS);
            }
            let slugs = match account {
                Some(a) => vec![resolve_account(&data, Some(&a))?],
                None => notedown_pipeline::setup::local_accounts(&data)?,
            };
            let local =
                notedown_cloud::localtime::local_utc_offset().unwrap_or(time::UtcOffset::UTC);
            let mut out = serde_json::Map::new();
            for slug in slugs {
                // No text store yet: no bookmarks, and nothing is created for the listing.
                let mut list = Vec::new();
                let dir = data
                    .join(notedown_pipeline::store::PIPELINE_DIR)
                    .join(&slug);
                if dir.is_dir() {
                    for (name, b) in TextStore::open(&data, &slug)?.consumers()? {
                        // A consumer that only read has no ack yet: no time.
                        let at = |ms: u64| (ms > 0).then(|| delta::stamp(ms, local));
                        list.push(serde_json::json!({
                            "consumer": name,
                            "seq": b.seq,
                            "updatedAt": at(b.updated_at),
                            "readAt": b.read_at.and_then(at),
                        }));
                    }
                }
                out.insert(slug, serde_json::Value::Array(list));
            }
            println!("{}", to_json(&out));
        }
        PipelineAction::Changes(a) => {
            let data = data_dir(a.data.clone())?;
            let slug = resolve_account(&data, a.account.as_deref())?;
            let store = TextStore::open(&data, &slug)?;
            let local =
                notedown_cloud::localtime::local_utc_offset().unwrap_or(time::UtcOffset::UTC);
            let start = match (a.consumer, a.since, a.from, a.up_to) {
                (Some(c), _, _, _) => Start::Bookmark(c),
                (_, Some(s), _, _) => Start::Since(delta::parse_since(
                    &s,
                    time::OffsetDateTime::now_utc(),
                    local,
                )?),
                (_, _, Some(from), Some(up_to)) => Start::Range { from, up_to },
                _ => return Err(anyhow!("give --consumer, --since, or --from with --up-to")),
            };
            if a.summary {
                println!("{}", to_json(&delta::manifest(&store, &start, local)?));
            } else {
                let d = delta::delta(&store, &start, a.notes.as_deref(), MAX_CHARS, local)?;
                println!("{}", to_json(&d));
            }
        }
        PipelineAction::Status { data, account } => {
            let data = data_dir(data)?;
            let slug = resolve_account(&data, account.as_deref())?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64);
            let facts = notedown_pipeline::status::status_facts(&data, &slug, now)?;
            println!("{}", to_json(&facts));
        }
        PipelineAction::Search {
            data,
            account,
            query,
            limit,
        } => {
            let data = data_dir(data)?;
            let slug = resolve_account(&data, account.as_deref())?;
            // `at` reads the store where it is and creates nothing: no store gives no hits.
            let store = TextStore::at(&data, &slug)?;
            let hits = notedown_pipeline::search::search(&store, &slug, &query, limit)?;
            println!("{}", to_json(&hits));
        }
        PipelineAction::Note { data, account, key } => {
            let data = data_dir(data)?;
            let slug = resolve_account(&data, account.as_deref())?;
            // `at` reads the store where it is and creates nothing.
            let store = TextStore::at(&data, &slug)?;
            let note = store
                .note(&key)?
                .filter(|note| !note.deleted)
                .ok_or_else(|| anyhow!("no note {key}"))?;
            print!("{}", notedown_pipeline::format::note_document(&note, &slug));
        }
        PipelineAction::Ack {
            data,
            account,
            consumer,
            up_to,
        } => {
            let data = data_dir(data)?;
            let slug = resolve_account(&data, account.as_deref())?;
            let store = TextStore::open(&data, &slug)?;
            let local =
                notedown_cloud::localtime::local_utc_offset().unwrap_or(time::UtcOffset::UTC);
            let b = store.ack(&consumer, up_to)?;
            let mut out = serde_json::json!({
                "seq": b.seq,
                "updatedAt": delta::stamp(b.updated_at, local),
            });
            // As the MCP ack says: an upTo past the log stops at its end.
            if b.seq < up_to {
                out["clamped"] = serde_json::Value::Bool(true);
                out["logEnd"] = serde_json::Value::from(store.last_seq()?);
            }
            println!("{}", to_json(&out));
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(feature = "ocr")]
#[derive(clap::Args)]
struct ConnectArgs {
    /// The server, for example http://pi.local:19072
    url: String,
    /// The pipeline's own data folder (it gets pipeline.toml and tokens/); default: $NOTEDOWN_DATA, else ~/notedown-data
    #[arg(long, value_name = "DIR")]
    data: Option<PathBuf>,
    /// The email of the account
    #[arg(long)]
    email: String,
    /// Read the password from the first line of stdin instead of asking for it
    #[arg(long)]
    password_stdin: bool,
    /// The name the token gets on the server
    #[arg(long, default_value = "pipeline")]
    name: String,
}

#[cfg(feature = "ocr")]
fn connect_command(a: ConnectArgs) -> Result<ExitCode> {
    let data = data_dir(a.data.clone())?;
    // A wrong folder or URL is said before the password is asked for.
    notedown_pipeline::connect::check(&a.url, &data)?;
    let password = read_password(a.password_stdin, false)?;
    let c = notedown_pipeline::connect::connect(&a.url, &data, &a.email, &password, &a.name)?;
    let url = a.url.trim().trim_end_matches('/');
    println!(
        "connected to {url} as {} (user id {}), token {} stored in {}",
        c.slug,
        c.user_id,
        c.token_id,
        c.token_file.display()
    );
    if let Some(old) = c.replaced_token_id {
        println!(
            "an older token {old} still works; revoke it on the server with notedown cloud \
             account tokens revoke"
        );
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(feature = "ocr")]
fn mcp_command(args: McpArgs) -> Result<ExitCode> {
    // serve_stdio builds its own runtime; this is plain synchronous code.
    notedown_pipeline::mcp::serve_stdio(&data_dir(args.data.clone())?, args.account.as_deref())?;
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        #[cfg(feature = "ocr")]
        Command::Convert(a) => convert(a),
        Command::Render(a) => render(a),
        #[cfg(feature = "ocr")]
        Command::Model { action } => model_command(action),
        Command::Serve(a) => serve_command(a),
        Command::Cloud { action } => cloud_command(action),
        #[cfg(feature = "ocr")]
        Command::Pipeline(a) => pipeline_command(a),
        #[cfg(feature = "ocr")]
        Command::Mcp(a) => mcp_command(a),
        #[cfg(feature = "ocr")]
        Command::Connect(a) => connect_command(a),
    };
    let _ = std::io::stdout().flush();
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            #[cfg(feature = "ocr")]
            if matches!(
                e.downcast_ref::<OcrError>(),
                Some(OcrError::ModelMissing { .. })
            ) {
                eprintln!("hint: run `notedown model pull` to download the model");
            }
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "ocr")]
    #[test]
    fn progress_json_has_file_done_and_total() {
        let v: serde_json::Value =
            serde_json::from_str(&super::progress_json("a.gguf", 5, 10)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"file": "a.gguf", "done": 5, "total": 10})
        );
        let r: serde_json::Value =
            serde_json::from_str(&super::ready_json(std::path::Path::new("/m/a.gguf"))).unwrap();
        assert_eq!(r, serde_json::json!({"ready": "/m/a.gguf"}));
    }

    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn resolve_data_prefers_the_argument() {
        let got = resolve_data(
            Some("/a".into()),
            Some("/env".into()),
            Some("/home/u".into()),
        )
        .unwrap();
        assert_eq!(got, PathBuf::from("/a"));
    }

    #[test]
    fn resolve_data_uses_the_environment() {
        let got = resolve_data(None, Some("/env".into()), Some("/home/u".into())).unwrap();
        assert_eq!(got, PathBuf::from("/env"));
    }

    #[test]
    fn resolve_data_ignores_an_empty_environment() {
        let got = resolve_data(None, Some("".into()), Some("/home/u".into())).unwrap();
        assert_eq!(got, PathBuf::from("/home/u/notedown-data"));
    }

    #[test]
    fn resolve_data_falls_back_to_home() {
        let got = resolve_data(None, None, Some("/home/u".into())).unwrap();
        assert_eq!(got, PathBuf::from("/home/u/notedown-data"));
    }

    #[test]
    fn resolve_data_without_any_source_is_an_error() {
        let err = resolve_data(None, None, None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "no data folder: pass --data or set NOTEDOWN_DATA"
        );
        let err = resolve_data(None, Some("".into()), Some("".into())).unwrap_err();
        assert!(err.to_string().starts_with("no data folder"));
    }

    #[test]
    fn dry_run_json_lists_folders_skips_and_needs_replace() {
        use notedown_cloud::import::{DryRun, FolderCount, Scan, Skip, SkipReason};
        let plan = DryRun {
            scan: Scan {
                folders: vec![FolderCount {
                    source: "Note".into(),
                    target: "NOTE/Note".into(),
                    files: 2,
                    folders: 1,
                    bytes: 30,
                }],
                skipped: vec![Skip {
                    path: "REMOVABLE0".into(),
                    reason: SkipReason::SdCard,
                }],
                errors: vec!["Note/a.part: unfinished".into()],
                ..Default::default()
            },
            files_present: 3,
        };
        let v: serde_json::Value =
            serde_json::from_str(&super::dry_run_json(&plan, false)).unwrap();
        let d = &v["dryRun"];
        assert_eq!(
            d["folders"],
            serde_json::json!([{"source": "Note", "target": "NOTE/Note", "files": 2, "folders": 1, "bytes": 30}])
        );
        assert_eq!(
            d["skipped"],
            serde_json::json!([{"path": "REMOVABLE0", "reason": "SD card"}])
        );
        assert_eq!(d["errors"], serde_json::json!(["Note/a.part: unfinished"]));
        assert_eq!(d["filesPresent"], 3);
        assert_eq!(d["needsReplace"], true);
        let v: serde_json::Value = serde_json::from_str(&super::dry_run_json(&plan, true)).unwrap();
        assert_eq!(v["dryRun"]["needsReplace"], false);
    }

    #[test]
    fn import_progress_json_lines() {
        use notedown_cloud::import::ImportProgress as P;
        let v =
            |p| serde_json::from_str::<serde_json::Value>(&super::import_progress_json(p)).unwrap();
        assert_eq!(
            v(P::Recycling { files: 7 }),
            serde_json::json!({"phase": "recycling", "files": 7})
        );
        assert_eq!(
            v(P::Imported {
                done: 100,
                total: 250
            }),
            serde_json::json!({"phase": "importing", "done": 100, "total": 250})
        );
        assert_eq!(v(P::Verifying), serde_json::json!({"phase": "verifying"}));
        assert!(super::import_progress_text(P::Imported { done: 0, total: 0 }, "a").is_none());
        // the human lines stay as they were: none for the closing report of an odd total
        assert_eq!(
            super::import_progress_text(
                P::Imported {
                    done: 100,
                    total: 250
                },
                "a"
            )
            .unwrap(),
            "imported 100 of 250 files"
        );
        assert!(
            super::import_progress_text(
                P::Imported {
                    done: 250,
                    total: 250
                },
                "a"
            )
            .is_none()
        );
    }

    #[test]
    fn app_data_json_for_the_empty_choice() {
        let report = notedown_cloud::appdata::AppImportReport::default();
        let v: serde_json::Value =
            serde_json::from_str(&super::app_data_json(&report, false)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"appData": {"dryRun": false, "digests": 0, "groups": 0, "marks": 0,
                "leftToDevice": [], "withoutGroup": [], "skipped": [], "replaced": 0}})
        );
    }

    #[test]
    fn one_sentence_keeps_the_first_line_and_names_scan_errors() {
        let e = super::one_sentence(
            "import: the backup cannot be imported completely, nothing was written:\n  a\n  b",
        );
        assert_eq!(e.to_string(), "the backup cannot be imported completely");
        let e = super::one_sentence("first\nsecond");
        assert_eq!(e.to_string(), "first");
        let e = super::one_sentence("the check failed:\n  a");
        assert_eq!(e.to_string(), "the check failed");
    }

    #[test]
    fn backup_progress_json_has_files_bytes_and_path() {
        assert_eq!(
            super::backup_progress_json(3, 2048, "Note/a.note"),
            r#"{"files":3,"bytes":2048,"path":"Note/a.note"}"#
        );
    }

    #[test]
    fn backup_report_json_wraps_the_report() {
        let report = notedown_cloud::device_backup::BackupReport {
            url: "http://d:8089".into(),
            time: "2026-11-17T10:00:00Z".into(),
            complete: true,
            folders: Default::default(),
            files: 0,
            bytes: 0,
            errors: Vec::new(),
            date_warnings: 0,
            warnings: Vec::new(),
            stale: Vec::new(),
            skipped_existing: 4,
        };
        let v: serde_json::Value =
            serde_json::from_str(&super::backup_report_json(&report)).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 1);
        assert_eq!(v["report"]["url"], "http://d:8089");
        assert_eq!(v["report"]["complete"], true);
        assert_eq!(v["report"]["stale"], serde_json::json!([]));
        assert!(v["report"].get("skipped_existing").is_none());
    }

    #[test]
    fn wildcard_listen_expands_to_host_addresses() {
        let listen = ["0.0.0.0:19072".parse().unwrap()];
        let ips = [Ipv4Addr::new(192, 168, 1, 20), Ipv4Addr::new(10, 0, 0, 5)];
        assert_eq!(
            device_urls(&listen, &ips),
            ["http://192.168.1.20:19072", "http://10.0.0.5:19072"]
        );
    }

    #[test]
    fn the_rest_base_is_never_an_unspecified_address() {
        let bound: Vec<std::net::SocketAddr> = vec![
            "0.0.0.0:19072".parse().unwrap(),
            "[::]:19072".parse().unwrap(),
        ];
        assert!(rest_bases(&bound, &[]).is_empty());
        let local = ["127.0.0.1:19072".parse().unwrap()];
        assert_eq!(rest_bases(&local, &[]), ["http://127.0.0.1:19072"]);
        let lan = ["http://192.168.1.20:19072".to_string()];
        assert_eq!(rest_bases(&bound, &lan), lan);
    }

    #[test]
    fn loopback_gives_no_device_url_and_a_lan_address_stays() {
        let ips = [Ipv4Addr::new(192, 168, 1, 20)];
        assert!(device_urls(&["127.0.0.1:19072".parse().unwrap()], &ips).is_empty());
        assert_eq!(
            device_urls(&["192.168.1.7:80".parse().unwrap()], &ips),
            ["http://192.168.1.7:80"]
        );
    }
}
