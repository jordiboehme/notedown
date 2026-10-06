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

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Convert(a) => convert(a),
        Command::Render(a) => render(a),
        Command::Model { action } => model_command(action),
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
