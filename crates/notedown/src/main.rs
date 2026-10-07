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
    /// Fill an account's digests from the device's .snbak backup and open digest and to-do sync
    /// (the backup holds no to-dos)
    ImportAppData(ImportAppDataArgs),
    /// Copy one file into an account (connected devices are told to sync)
    Put(PutArgs),
    /// Copy every file of the device over Browse & Access (port 8089) into a folder
    BackupDevice(BackupDeviceArgs),
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
    /// Clear the digests and to-dos the account holds now first
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
    eprintln!(
        "done: {} converted, {} skipped, {} failed ({}s)",
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
    for id in &report.new_ids {
        println!("no cloud id on the device, new id: {id}");
    }
    for name in &report.skipped {
        println!("skipped {name} (no digest names it)");
    }
    if report.replaced > 0 {
        println!(
            "{} {} of the account {}",
            if args.dry_run {
                "would clear"
            } else {
                "cleared"
            },
            plural(report.replaced as usize, "record"),
            args.account
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
    )?;
    let what = match r.outcome {
        PutOutcome::Created => "created",
        PutOutcome::Replaced => "replaced, the old version is in the recycle bin",
        PutOutcome::Unchanged => "unchanged, the same bytes are there already",
    };
    println!("{} id {} ({what})", r.path, r.id);
    Ok(ExitCode::SUCCESS)
}

/// `+HH:MM`, `-HH:MM`, `+HHMM` or `Z`.
fn parse_utc_offset(text: &str) -> Option<time::UtcOffset> {
    let t = text.trim();
    if t == "Z" || t == "z" {
        return Some(time::UtcOffset::UTC);
    }
    let (sign, rest) = match t.as_bytes().first()? {
        b'+' => (1i8, &t[1..]),
        b'-' => (-1i8, &t[1..]),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|c| *c != ':').collect();
    if digits.len() != 4 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let h: i8 = digits[..2].parse().ok()?;
    let m: i8 = digits[2..].parse().ok()?;
    time::UtcOffset::from_hms(sign * h, sign * m, 0).ok()
}

/// This computer's current offset from UTC, from `date +%z` (the time crate cannot read it
/// safely once threads run).
fn local_utc_offset() -> Option<time::UtcOffset> {
    let out = std::process::Command::new("date")
        .arg("+%z")
        .output()
        .ok()?;
    parse_utc_offset(std::str::from_utf8(&out.stdout).ok()?)
}

fn backup_device_command(args: BackupDeviceArgs) -> Result<ExitCode> {
    use notedown_cloud::device_backup::{BackupOptions, backup};
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
            let store = notedown_cloud::store::Store::open(data, &account)?;
            let guard = store.lock_recovered(&journal)?;
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
    fn utc_offsets_parse_in_the_usual_forms() {
        let two = time::UtcOffset::from_hms(2, 0, 0).unwrap();
        assert_eq!(parse_utc_offset("+02:00"), Some(two));
        assert_eq!(parse_utc_offset("+0200\n"), Some(two));
        assert_eq!(
            parse_utc_offset("-05:30"),
            Some(time::UtcOffset::from_hms(-5, -30, 0).unwrap())
        );
        assert_eq!(parse_utc_offset("Z"), Some(time::UtcOffset::UTC));
        for bad in ["", "02:00", "+2", "+02:0x", "CEST"] {
            assert_eq!(parse_utc_offset(bad), None, "{bad}");
        }
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
