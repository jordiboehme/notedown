use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use notedown::inputs;
use notedown::links::FileIndex;
use notedown::pipeline::{Options, Outcome, convert_one};
use notedown_note::{Notebook, RenderOptions, VersionPolicy, encode_png};
use notedown_ocr::model::{self, DEFAULT_MODEL};
use notedown_ocr::{LlamaEngine, OcrEngine, OcrError, OcrOptions, PageOcr};

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
    Convert(ConvertArgs),
    /// Render the pages of a .note file as PNG images
    Render(RenderArgs),
    /// Manage the OCR model files
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
}

#[derive(clap::Args)]
struct ServeArgs {
    /// The data folder (server.toml, accounts, logs)
    #[arg(long, value_name = "DIR")]
    data: PathBuf,
    /// Also write every request and frame, masked, to <data>/capture/ (this run only)
    #[arg(long)]
    capture: bool,
    /// Refuse every sync, whatever server.toml says: capture only, no files are exchanged (this
    /// run only)
    #[arg(long)]
    refuse_sync: bool,
}

#[derive(Subcommand)]
enum CloudAction {
    /// Manage the accounts of a data folder
    Account {
        /// The data folder
        #[arg(long, value_name = "DIR")]
        data: PathBuf,
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
}

#[derive(clap::Args)]
struct ScanArgs {
    /// The data folder
    #[arg(long, value_name = "DIR")]
    data: PathBuf,
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
    /// The data folder
    #[arg(long, value_name = "DIR")]
    data: PathBuf,
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
    /// The data folder
    #[arg(long, value_name = "DIR")]
    data: PathBuf,
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
}

#[derive(clap::Args)]
struct ImportAppDataArgs {
    /// The device backup (.snbak, made by the device's own backup function)
    #[arg(value_name = "SNBAK", required_unless_present = "empty")]
    snbak: Option<PathBuf>,
    /// The data folder
    #[arg(long, value_name = "DIR")]
    data: PathBuf,
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
}

#[derive(clap::Args)]
struct PutArgs {
    /// The file to copy
    file: PathBuf,
    /// The data folder
    #[arg(long, value_name = "DIR")]
    data: PathBuf,
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
}

#[derive(Subcommand)]
enum JournalAction {
    /// Print the journal events as JSON lines
    Tail {
        /// The data folder
        #[arg(long, value_name = "DIR")]
        data: PathBuf,
        /// Start at this seq (default: the oldest kept event)
        #[arg(long, value_name = "SEQ")]
        from: Option<u64>,
        /// Keep running and print new events as they come (Ctrl-C stops)
        #[arg(long)]
        follow: bool,
    },
    /// Journal the current files, folders, digests and to-dos of an account as created events
    Snapshot {
        /// The data folder
        #[arg(long, value_name = "DIR")]
        data: PathBuf,
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
    /// Delete an account with all its files
    Remove {
        slug: String,
        /// Confirm the deletion
        #[arg(long)]
        yes: bool,
    },
}

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
}

#[derive(Subcommand)]
enum ModelAction {
    /// Download the model files into the cache
    Pull,
    /// Print the cache folder and the model file paths
    Path,
    /// Check size and SHA-256 of the model files
    Verify,
}

/// Loads the model on the first page that needs OCR, so an up-to-date folder never loads it.
struct LazyEngine {
    label: String,
    model: Option<PathBuf>,
    mmproj: Option<PathBuf>,
    opts: OcrOptions,
    engine: Option<LlamaEngine>,
    load_error: Option<String>,
    loaded_in: Option<Duration>,
}

impl OcrEngine for LazyEngine {
    fn ocr_png(&mut self, png: &[u8]) -> Result<PageOcr, OcrError> {
        if self.engine.is_none() {
            if let Some(msg) = &self.load_error {
                return Err(OcrError::Engine(msg.clone()));
            }
            let start = Instant::now();
            let loaded = model::resolve(
                &DEFAULT_MODEL,
                self.model.as_deref(),
                self.mmproj.as_deref(),
            )
            .and_then(|paths| {
                eprintln!("loading model {} ...", paths.label);
                LlamaEngine::load(&paths, self.opts)
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

fn display_name(rel: &Path) -> String {
    rel.to_string_lossy().into_owned()
}

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
    let label = match &args.model {
        Some(p) => p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| DEFAULT_MODEL.label.to_string()),
        None => DEFAULT_MODEL.label.to_string(),
    };
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
    let kind_ok = args
        .file
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("note"));
    if !kind_ok {
        return Err(anyhow!("{} is not a .note file", args.file.display()));
    }
    let nb = Notebook::open(&args.file, VersionPolicy::Strict)
        .with_context(|| format!("cannot read {}", args.file.display()))?;
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
    for i in 0..nb.pages.len() {
        let img = nb
            .render_page(i, &opts)
            .with_context(|| format!("page {}", i + 1))?;
        let path = dir.join(format!("{stem}-page-{:03}.png", i + 1));
        std::fs::write(&path, encode_png(&img)?)
            .with_context(|| format!("cannot write {}", path.display()))?;
        println!("{}", path.display());
    }
    Ok(ExitCode::SUCCESS)
}

fn model_command(action: ModelAction) -> Result<ExitCode> {
    match action {
        ModelAction::Pull => {
            let mut last = Instant::now() - Duration::from_secs(10);
            let mut current = String::new();
            let mut progress = |file: &str, done: u64, total: u64| {
                let new_file = file != current;
                if new_file {
                    current = file.to_string();
                }
                let finished = total > 0 && done >= total;
                if new_file || finished || last.elapsed() >= Duration::from_secs(2) {
                    last = Instant::now();
                    let pct = (done * 100).checked_div(total).unwrap_or(0);
                    eprintln!("downloading {file}: {pct}%");
                }
            };
            let paths = model::pull(&DEFAULT_MODEL, &mut progress)?;
            eprintln!("model ready: {}", paths.model.display());
            Ok(ExitCode::SUCCESS)
        }
        ModelAction::Path => {
            let dir = model::cache_dir()?;
            println!("cache: {}", dir.display());
            println!(
                "model: {}",
                dir.join(DEFAULT_MODEL.model.file_name).display()
            );
            println!(
                "mmproj: {}",
                dir.join(DEFAULT_MODEL.mmproj.file_name).display()
            );
            Ok(ExitCode::SUCCESS)
        }
        ModelAction::Verify => {
            let dir = model::cache_dir()?;
            let mut bad = false;
            for file in [&DEFAULT_MODEL.model, &DEFAULT_MODEL.mmproj] {
                match model::verify_file(&dir.join(file.file_name), file) {
                    Ok(()) => println!("{}: ok", file.file_name),
                    Err(e) => {
                        bad = true;
                        println!("{}: {e}", file.file_name);
                    }
                }
            }
            Ok(if bad {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            })
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
            recover_accounts(&data)?;
            account_command(&data, action)
        }
        CloudAction::Journal { action } => {
            let data = match &action {
                JournalAction::Tail { data, .. } | JournalAction::Snapshot { data, .. } => {
                    data.clone()
                }
            };
            recover_accounts(&data)?;
            journal_command(&data, action)
        }
        CloudAction::Import(args) => {
            if !args.dry_run {
                recover_accounts(&args.data)?;
            }
            import_command(args)
        }
        CloudAction::ImportAppData(args) => {
            if !args.dry_run {
                recover_accounts(&args.data)?;
            }
            import_app_data_command(args)
        }
        CloudAction::Put(args) => {
            recover_accounts(&args.data)?;
            put_command(args)
        }
        CloudAction::BackupDevice(args) => backup_device_command(args),
        CloudAction::Recycle {
            action: RecycleAction::Prune(args),
        } => {
            if !args.dry_run {
                recover_accounts(&args.data)?;
            }
            recycle_prune_command(args)
        }
        CloudAction::Scan(args) => {
            if !args.dry_run {
                recover_accounts(&args.data)?;
            }
            scan_command(args)
        }
    }
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

fn import_command(args: ImportArgs) -> Result<ExitCode> {
    use notedown_cloud::import;
    let options = import::ImportOptions {
        replace: args.replace,
        no_report: args.no_report,
        allow_empty: args.allow_empty,
    };
    if args.dry_run {
        let plan = import::dry_run(&args.data, &args.account, &args.backup, &options)?;
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
        let empty_refused = plan.scan.files() == 0 && !args.allow_empty;
        if empty_refused {
            println!("error: the backup holds no files; the import needs --allow-empty");
        }
        return Ok(if plan.scan.errors.is_empty() && !empty_refused {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        });
    }
    let journal = notedown_cloud::journal::Journal::open(&args.data)?;
    let report = import::import(
        &args.data,
        &args.account,
        &args.backup,
        &options,
        &journal,
        &mut |line| eprintln!("{line}"),
    )?;
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
    use notedown_cloud::appdata::{AppImportOptions, dry_run_app_data, import_app_data};
    let report = if args.dry_run {
        // No lock and no journal: a dry run writes nothing, not even a folder.
        dry_run_app_data(
            &args.data,
            &args.account,
            args.snbak.as_deref(),
            args.replace,
        )?
    } else {
        let journal = notedown_cloud::journal::Journal::open(&args.data)?;
        import_app_data(
            &args.data,
            &args.account,
            args.snbak.as_deref(),
            &AppImportOptions {
                dry_run: false,
                replace: args.replace,
            },
            &journal,
        )?
    };
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
    use notedown_cloud::put::{PutOutcome, put};
    let journal = notedown_cloud::journal::Journal::open(&args.data)?;
    let r = put(
        &args.data,
        &args.account,
        &journal,
        &args.file,
        &args.to,
        args.name.as_deref(),
    )
    .map_err(|e| held_error(&args.data, &args.account, e))?;
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
    use notedown_cloud::journal::{Journal, Origin};
    use notedown_cloud::store::{RecycleReason, Store};
    // A dry run leaves the registry log as it is.
    let store = if args.dry_run {
        Store::open_no_compact(&args.data, &args.account)?
    } else {
        Store::open(&args.data, &args.account)?
    };
    // Never write server.toml here, and a dry run creates nothing at all.
    let config = notedown_cloud::Config::load_or_default(&args.data)?;
    let journal = if args.dry_run {
        Journal::open_no_create(&args.data)
    } else {
        Journal::open(&args.data)?
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
    use notedown_cloud::journal::Journal;
    if !args.data.is_dir() {
        return Err(anyhow!(
            "data folder {} does not exist",
            args.data.display()
        ));
    }
    // Never write server.toml here, and a dry run creates nothing at all.
    let config = notedown_cloud::Config::load_or_default(&args.data)?;
    let journal = if args.dry_run {
        Journal::open_no_create(&args.data)
    } else {
        Journal::open(&args.data)?
    };
    let slugs = match &args.account {
        Some(slug) => vec![slug.clone()],
        None => notedown_cloud::auth::Accounts::new(&args.data)
            .list()?
            .into_iter()
            .map(|a| a.slug)
            .collect(),
    };
    let mut failed = false;
    for slug in &slugs {
        // With --account the error ends the command; otherwise the other accounts still run.
        match scan_account(&args.data, slug, &args, config.scan_min_files, &journal) {
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
    let report = backup(&args.url, &args.dir, &options, &mut |line| {
        eprintln!("{line}")
    })?;
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
            accounts.set_password(&slug, &password)?;
            println!("password of {slug} changed");
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
                    "this deletes account {slug} with all its files; add --yes to confirm"
                ));
            }
            accounts.remove(&slug)?;
            println!("removed account {slug}");
        }
    }
    Ok(ExitCode::SUCCESS)
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
    use notedown_cloud::Config;
    use notedown_cloud::server::{AppState, bind, serve_until};
    let mut config = Config::load_or_create(&args.data)?;
    // The flag counts for this run only; server.toml is not rewritten.
    let capture_from_file = config.capture && !args.capture;
    config.capture |= args.capture;
    if args.refuse_sync {
        config.sync = notedown_cloud::SyncMode::Refuse;
    }
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let listeners = bind(&config).await?;
        let state = AppState::open(config.clone(), &args.data)?;
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
        for url in urls {
            println!("device URL: {url}");
        }
        if let Some(url) = &config.external_base_url {
            println!("external base URL: {url}");
        }
        println!("data folder: {}", args.data.display());
        println!("log folder: {}", state.log.logs_dir().display());
        if config.capture {
            println!("capture dumps: {}/capture", args.data.display());
        }
        if capture_from_file {
            eprintln!(
                "warning: capture = true is set in server.toml. Capture is for one capture \
                 session only, the dumps have no size cap. Set capture = false and use --capture."
            );
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
                        notedown_cloud::PushMode::Reconnect => {
                            "reconnect (the device's socket is closed, it syncs when it connects again)"
                        }
                        notedown_cloud::PushMode::Startsync => "startsync",
                        notedown_cloud::PushMode::Detail => "detail",
                        notedown_cloud::PushMode::Off => "off (devices sync on their own schedule)",
                    }
                );
            }
        }
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
        println!("press Ctrl-C to stop");
        std::io::stdout().flush()?;
        serve_until(listeners, state.clone(), async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Convert(a) => convert(a),
        Command::Render(a) => render(a),
        Command::Model { action } => model_command(action),
        Command::Serve(a) => serve_command(a),
        Command::Cloud { action } => cloud_command(action),
    };
    let _ = std::io::stdout().flush();
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
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
    use super::*;
    use std::net::Ipv4Addr;

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
    fn loopback_gives_no_device_url_and_a_lan_address_stays() {
        let ips = [Ipv4Addr::new(192, 168, 1, 20)];
        assert!(device_urls(&["127.0.0.1:19072".parse().unwrap()], &ips).is_empty());
        assert_eq!(
            device_urls(&["192.168.1.7:80".parse().unwrap()], &ips),
            ["http://192.168.1.7:80"]
        );
    }
}
