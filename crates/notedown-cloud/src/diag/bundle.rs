//! `notedown cloud diagnose` (spec 1 and 2): the report and the zip. Every line goes through
//! the filter and, in a safe bundle, through the final check before it is written. The zip is
//! written to a temp file next to `--out` and renamed at the end, so a stopped or failed run
//! leaves no zip behind.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::check::{Leak, Needles, check_json, check_values};
use super::collect::{Window, is_heartbeat, journal_ts, log_files, log_ts, read_lines};
use super::filter::{Filter, Scrub};
use super::report::{PipelineCounts, ReportInput, Stats, render};
use super::standin::StandIns;
use super::state::{AccountState, ScanResult, account_state, collect_needles, dry_run_scan};
use crate::auth::{Accounts, Devices};
use crate::config::{CONFIG_FILE, Config};
use crate::journal::Journal;
use crate::{Error, Result, io_err};

/// How long the dry-run scan waits for a busy store lock at most.
pub const LOCK_WAIT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct DiagnoseOptions {
    pub data: PathBuf,
    /// The start of the window; it ends now.
    pub since: OffsetDateTime,
    pub device: Option<String>,
    pub account: Option<String>,
    pub private: bool,
    pub out: PathBuf,
    /// How long the dry-run scan waits for the store lock ([`LOCK_WAIT`]).
    pub lock_wait: Duration,
    /// Test hook: the filter keeps this key as it is, so the final check must stop the bundle.
    #[doc(hidden)]
    pub test_keep_key: Option<String>,
}

/// What [`diagnose`] made.
#[derive(Debug, Clone)]
pub struct Diagnosis {
    pub report: String,
    pub zip: PathBuf,
    pub bytes: u64,
}

/// Everything read before the bundle is written; the same for a second run after a clash.
struct Facts {
    window: Window,
    now: OffsetDateTime,
    selected: Vec<String>,
    states: Vec<AccountState>,
    scans: Vec<(String, ScanResult)>,
    render_cursor: Option<u64>,
    render_behind: Option<u64>,
    pipeline: PipelineCounts,
    needles: Needles,
    scrub: Scrub,
    /// Device serials bound to each account, for `--account` on log lines.
    bound: BTreeMap<String, BTreeSet<String>>,
    fs: String,
    names_preserved: bool,
}

/// Make the report and the zip.
pub fn diagnose(opts: &DiagnoseOptions) -> Result<Diagnosis> {
    if opts.out.exists() {
        return Err(Error::Invalid {
            what: "--out",
            message: format!("{} already exists", opts.out.display()),
        });
    }
    let data = &opts.data;
    if !data.is_dir() || !(data.join(CONFIG_FILE).is_file() || data.join("accounts").is_dir()) {
        return Err(Error::Invalid {
            what: "--data",
            message: format!(
                "{} is no notedown data folder (no server.toml and no accounts/)",
                data.display()
            ),
        });
    }
    let facts = read_facts(opts)?;
    let mut key = [0u8; 32];
    rand::fill(&mut key[..]);
    match write_bundle(opts, &facts, StandIns::with_key(key, 4))? {
        Some(done) => Ok(done),
        // Two names got the same short stand-in: every stand-in of the bundle gets 8.
        None => {
            write_bundle(opts, &facts, StandIns::with_key(key, 8))?.ok_or_else(|| Error::Invalid {
                what: "diagnose bundle",
                message: "two names got the same stand-in; nothing was written".into(),
            })
        }
    }
}

fn read_facts(opts: &DiagnoseOptions) -> Result<Facts> {
    let data = &opts.data;
    let now = OffsetDateTime::now_utc();
    let window = Window {
        from: opts.since.min(now),
        to: now,
    };
    let all: Vec<String> = Accounts::new(data)
        .list()?
        .into_iter()
        .map(|a| a.slug)
        .collect();
    if let Some(slug) = &opts.account
        && !all.contains(slug)
    {
        return Err(Error::Invalid {
            what: "--account",
            message: format!(
                "there is no account {slug} in {}",
                data.join("accounts").display()
            ),
        });
    }
    let selected: Vec<String> = match &opts.account {
        Some(slug) => vec![slug.clone()],
        None => all.clone(),
    };
    let devices = Devices::new(data);
    let mut bound: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for slug in &all {
        for d in devices.list(slug)? {
            bound.entry(slug.clone()).or_default().insert(d.serial);
        }
    }
    if let Some(serial) = &opts.device
        && !bound.values().any(|s| s.contains(serial))
        && !device_in_log(data, &window, serial)?
    {
        return Err(Error::Invalid {
            what: "--device",
            message: format!("device {serial} is in no account and not in the log of the window"),
        });
    }
    let mut needles = Needles::new();
    let mut scrub = Scrub::default();
    // The real values of every account, also with --account: a line of another account
    // must not leak either.
    collect_needles(data, &all, &mut needles, &mut scrub)?;

    // Journal facts: the last seq per account and how far the render follower is behind.
    let journal = Journal::open_no_create(data);
    let render_cursor = journal.cursor("render").ok().flatten().map(|c| c.seq);
    let mut last_seq: BTreeMap<String, u64> = BTreeMap::new();
    let mut behind: BTreeMap<String, u64> = BTreeMap::new();
    let mut behind_all = 0;
    for (_, path) in journal.files().unwrap_or_default() {
        read_lines(&path, &mut |v| {
            // Names of the journal's history (also of files gone since) are real values too.
            for key in ["path", "oldPath", "recycle"] {
                if let Some(p) = v.get(key).and_then(Value::as_str) {
                    needles.add_path(p);
                    scrub.names.extend(p.split('/').map(String::from));
                }
            }
            let seq = v.get("seq").and_then(Value::as_u64).unwrap_or(0);
            let account = v.get("account").and_then(Value::as_str).unwrap_or_default();
            let last = last_seq.entry(account.to_string()).or_default();
            *last = (*last).max(seq);
            if render_cursor.is_some_and(|c| seq > c) {
                behind_all += 1;
                *behind.entry(account.to_string()).or_default() += 1;
            }
            Ok(())
        })?;
    }
    scrub
        .names
        .retain(|n| !n.is_empty() && !n.bytes().all(|b| b.is_ascii_digit()));
    scrub.names.sort();
    scrub.names.dedup();
    let config = Config::load_or_default(data).unwrap_or_default();
    let mut states = Vec::new();
    let mut scans = Vec::new();
    for slug in &selected {
        let mut s = account_state(data, slug)?;
        s.last_seq = last_seq.get(slug).copied().unwrap_or(0);
        s.render_behind = render_cursor.map(|_| behind.get(slug).copied().unwrap_or(0));
        states.push(s);
        scans.push((
            slug.clone(),
            dry_run_scan(data, slug, config.scan_min_files, opts.lock_wait),
        ));
    }
    let pipeline = pipeline_counts(data, &selected, &journal);
    let probe_dir = if data.join("accounts").is_dir() {
        data.join("accounts")
    } else {
        data.to_path_buf()
    };
    Ok(Facts {
        window,
        now,
        selected,
        states,
        scans,
        render_cursor,
        render_behind: render_cursor.map(|_| behind_all),
        pipeline,
        needles,
        scrub,
        bound,
        fs: file_system(data),
        names_preserved: crate::store::probe_names_preserved(&probe_dir),
    })
}

/// The pipeline's journal cursor and the pages of the selected accounts' note states that are
/// pending or failed. Only numbers leave this function. A missing folder counts nothing; a
/// note state that cannot be read is skipped. A page without a state is pending (the
/// pipeline's own default); the pages of a deleted note are no work and are not counted.
fn pipeline_counts(data: &Path, selected: &[String], journal: &Journal) -> PipelineCounts {
    let mut counts = PipelineCounts {
        cursor: journal.cursor("pipeline").ok().flatten().map(|c| c.seq),
        ..PipelineCounts::default()
    };
    for slug in selected {
        let dir = data.join("pipeline").join(slug).join("notes");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(note) = std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            else {
                continue;
            };
            if note.get("deleted").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let pages = note.get("pages").and_then(Value::as_array);
            for page in pages.into_iter().flatten() {
                match page
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("pending")
                {
                    "pending" => counts.pending += 1,
                    "failed" => counts.failed += 1,
                    _ => {}
                }
            }
        }
    }
    counts
}

fn device_in_log(data: &Path, window: &Window, serial: &str) -> Result<bool> {
    let mut found = false;
    for path in log_files(&data.join("logs"), window)? {
        read_lines(&path, &mut |v| {
            if v.get("device").and_then(Value::as_str) == Some(serial) {
                found = true;
            }
            Ok(())
        })?;
    }
    Ok(found)
}

/// Removes the temp zip unless it was renamed.
struct TempZip {
    path: PathBuf,
    keep: bool,
}

impl Drop for TempZip {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

type Zip = zip::ZipWriter<std::io::BufWriter<std::fs::File>>;

/// The bundle writer: the zip, the filter and the final check.
struct Writer<'a> {
    zip: Zip,
    path: PathBuf,
    filter: Filter,
    needles: &'a Needles,
    private: bool,
}

impl Writer<'_> {
    fn start(&mut self, name: &str) -> Result<()> {
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        self.zip
            .start_file(name, options)
            .map_err(|e| io_err(&self.path)(std::io::Error::other(e)))
    }

    fn bytes(&mut self, b: &[u8]) -> Result<()> {
        self.zip.write_all(b).map_err(io_err(&self.path))
    }

    /// The final check of one value; a hit stops the bundle.
    fn check(&self, leak: Option<Leak>) -> Result<()> {
        match leak {
            Some(leak) if !self.private => Err(Error::Invalid {
                what: "diagnose bundle",
                message: format!(
                    "the safe bundle would show a real name in {}, key {}; nothing was written",
                    leak.file, leak.key
                ),
            }),
            _ => Ok(()),
        }
    }

    fn json_line(&mut self, file: &str, v: &Value, route: bool) -> Result<()> {
        self.check(check_json(self.needles, file, v, route))?;
        let mut line = serde_json::to_vec(v).expect("a JSON value serializes");
        line.push(b'\n');
        self.bytes(&line)
    }

    fn json_file(&mut self, file: &str, v: &Value) -> Result<()> {
        self.check(check_json(self.needles, file, v, false))?;
        self.start(file)?;
        let mut text = serde_json::to_vec_pretty(v).expect("a JSON value serializes");
        text.push(b'\n');
        self.bytes(&text)
    }
}

/// Write the bundle; `None` when two names got the same stand-in (nothing is kept then).
fn write_bundle(
    opts: &DiagnoseOptions,
    facts: &Facts,
    standins: StandIns,
) -> Result<Option<Diagnosis>> {
    let data = &opts.data;
    let mut filter = Filter::new(opts.private, standins, facts.scrub.clone());
    if let Some(k) = &opts.test_keep_key {
        filter.keep_key_for_test(k);
    }
    let name = opts
        .out
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "bundle.zip".into());
    let dir = match opts.out.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let file = std::fs::File::create(&tmp).map_err(io_err(&tmp))?;
    let mut guard = TempZip {
        path: tmp.clone(),
        keep: false,
    };
    let mut w = Writer {
        zip: zip::ZipWriter::new(std::io::BufWriter::new(file)),
        path: tmp.clone(),
        filter,
        needles: &facts.needles,
        private: opts.private,
    };
    let mut stats = Stats::default();

    // protocol.jsonl
    w.start("protocol.jsonl")?;
    for path in log_files(&data.join("logs"), &facts.window)? {
        let bad = read_lines(&path, &mut |v| {
            if !log_ts(&v).is_some_and(|t| facts.window.contains(t)) || !keep_log(opts, facts, &v) {
                return Ok(());
            }
            stats.observe_log(&v);
            if is_heartbeat(&v) {
                return Ok(());
            }
            let f = w.filter.log_line(&v);
            w.json_line("protocol.jsonl", &f, true)
        })?;
        stats.unreadable += bad;
    }

    // journal.jsonl
    w.start("journal.jsonl")?;
    let journal = Journal::open_no_create(data);
    for (_, path) in journal.files().unwrap_or_default() {
        let bad = read_lines(&path, &mut |v| {
            if !journal_ts(&v).is_some_and(|t| facts.window.contains(t)) || !keep_journal(opts, &v)
            {
                return Ok(());
            }
            stats.observe_journal(&v);
            let f = w.filter.journal_line(&v);
            w.json_line("journal.jsonl", &f, false)
        })?;
        stats.unreadable += bad;
    }

    // accounts/<account>.json
    for s in &facts.states {
        let acct = w.filter.account(&s.slug);
        let devices: Vec<Value> = s
            .devices
            .iter()
            .map(|d| {
                json!({"serial": w.filter.device(&d.serial), "firmware": d.firmware,
                    "bound": d.bound, "last_login_ms": d.last_login_ms})
            })
            .collect();
        let v = json!({
            "account": acct,
            "files": s.files, "folders": s.folders, "notes": s.notes, "bytes": s.bytes,
            "recycle": {"entries": s.recycle_entries, "bytes": s.recycle_bytes},
            "digests": s.digests, "digest_groups": s.digest_groups, "marks": s.marks,
            "todo_groups": s.todo_groups, "todos": s.todos,
            "devices": devices,
            "scan_hold": s.hold.as_ref().map(|h| json!({"at": h.at, "gone": h.gone, "left": h.left, "min": h.min})),
            "scan_hold_unreadable": s.hold_unreadable,
            "unfinished_change": s.pending,
            "journal": {"last_seq": s.last_seq, "render_cursor": facts.render_cursor,
                "render_behind": s.render_behind},
        });
        w.json_file(&format!("accounts/{acct}.json"), &v)?;
    }

    // scan.json
    let mut scans = Vec::new();
    for (slug, scan) in &facts.scans {
        let acct = w.filter.account(slug);
        let v = match scan {
            ScanResult::Skipped(why) => json!({"account": acct, "skipped": w.filter.text(why)}),
            ScanResult::Done(r) => {
                let mut paths = |list: &mut dyn Iterator<Item = String>| -> Vec<Value> {
                    list.map(|p| Value::String(w.filter.path(&p))).collect()
                };
                let created = paths(&mut r.created.iter().map(|p| p.display()));
                let modified = paths(&mut r.modified.iter().map(|p| p.display()));
                let deleted = paths(&mut r.deleted.iter().map(|p| p.display()));
                let moved: Vec<Value> = r
                    .moved
                    .iter()
                    .map(|(a, b)| json!([w.filter.path(&a.display()), w.filter.path(&b.display())]))
                    .collect();
                json!({"account": acct, "created": created, "modified": modified,
                    "moved": moved, "deleted": deleted,
                    "would_hold": r.held.as_ref().map(|h| json!({"gone": h.gone, "left": h.left, "min": h.min})),
                    "would_release_hold": r.released})
            }
        };
        scans.push(v);
    }
    w.json_file("scan.json", &json!({"accounts": scans}))?;

    // server.toml
    if let Ok(text) = std::fs::read_to_string(data.join(CONFIG_FILE)) {
        let out = match w.filter.server_toml(&text) {
            Ok(t) => t,
            Err(_) if opts.private => text,
            Err(_) => "# server.toml cannot be read as TOML; it is left out\n".into(),
        };
        if let Ok(table) = toml::from_str::<toml::Table>(&out) {
            let v = serde_json::to_value(&table).unwrap_or(Value::Null);
            w.check(check_json(&facts.needles, "server.toml", &v, false))?;
        }
        w.start("server.toml")?;
        w.bytes(out.as_bytes())?;
    }

    // about.json
    let device = opts.device.as_deref().map(|d| w.filter.device(d));
    let account = opts.account.as_deref().map(|a| w.filter.account(a));
    let about = json!({
        "notedown": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "data_folder": {"file_system": facts.fs, "keeps_name_spelling": facts.names_preserved},
        "window": {"from": rfc3339(facts.window.from), "to": rfc3339(facts.window.to)},
        "filters": {"device": device, "account": account},
        "accounts": facts.selected.len(),
        "private": opts.private,
        "made": rfc3339(facts.now),
    });
    w.json_file("about.json", &about)?;

    // report.txt
    let input = ReportInput {
        version: env!("CARGO_PKG_VERSION"),
        now: facts.now,
        window: &facts.window,
        private: opts.private,
        fs: &facts.fs,
        names_preserved: facts.names_preserved,
        accounts: &facts.states,
        scans: &facts.scans,
        render_behind: facts.render_behind,
        pipeline: facts.pipeline,
    };
    let (report, values) = render(&stats, &input, &mut w.filter);
    w.check(check_values(
        &facts.needles,
        "report.txt",
        values.iter().map(String::as_str),
    ))?;
    w.start("report.txt")?;
    w.bytes(report.as_bytes())?;

    if w.filter.standins.clashed() && !opts.private {
        return Ok(None);
    }
    let file = w
        .zip
        .finish()
        .map_err(|e| io_err(&tmp)(std::io::Error::other(e)))?;
    let file = file
        .into_inner()
        .map_err(|e| io_err(&tmp)(e.into_error()))?;
    file.sync_all().map_err(io_err(&tmp))?;
    drop(file);
    if opts.out.exists() {
        return Err(Error::Invalid {
            what: "--out",
            message: format!("{} already exists", opts.out.display()),
        });
    }
    std::fs::rename(&tmp, &opts.out).map_err(io_err(&opts.out))?;
    guard.keep = true;
    let bytes = std::fs::metadata(&opts.out).map_or(0, |m| m.len());
    Ok(Some(Diagnosis {
        report,
        zip: opts.out.clone(),
        bytes,
    }))
}

/// `--device` and `--account` on a protocol log line.
fn keep_log(opts: &DiagnoseOptions, facts: &Facts, v: &Value) -> bool {
    let device = v.get("device").and_then(Value::as_str);
    if let Some(serial) = &opts.device
        && device != Some(serial.as_str())
    {
        return false;
    }
    if let Some(slug) = &opts.account {
        return match device {
            Some(d) => facts.bound.get(slug).is_some_and(|s| s.contains(d)),
            // A server marker stays unless it names another account.
            None if v.get("kind").and_then(Value::as_str) == Some("marker") => {
                let message = v.get("message").and_then(Value::as_str).unwrap_or_default();
                !facts
                    .bound
                    .keys()
                    .chain(facts.selected.iter())
                    .any(|other| other != slug && message.contains(&format!("account {other}")))
            }
            None => false,
        };
    }
    true
}

/// `--device` and `--account` on a journal event. Events of other origins (scan, CLI) have no
/// device and stay with `--device`.
fn keep_journal(opts: &DiagnoseOptions, v: &Value) -> bool {
    if let Some(serial) = &opts.device
        && let Some(d) = v.get("device").and_then(Value::as_str)
        && d != serial
    {
        return false;
    }
    if let Some(slug) = &opts.account
        && v.get("account").and_then(Value::as_str) != Some(slug.as_str())
    {
        return false;
    }
    true
}

fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_default()
}

/// The file system type of `dir`: `stat -f -c %T` on Linux, the `mount` table on macOS,
/// otherwise `unknown`.
fn file_system(dir: &Path) -> String {
    let run = |cmd: &str, args: &[&str]| -> Option<String> {
        let out = std::process::Command::new(cmd).args(args).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let path = dir.to_string_lossy();
    if cfg!(target_os = "linux") {
        if let Some(t) = run("stat", &["-f", "-c", "%T", &path]) {
            return t.trim().to_string();
        }
    } else if cfg!(target_os = "macos") {
        // `df -P` names the device of the folder; `mount` gives its type in parentheses.
        let device = run("df", &["-P", &path]).and_then(|t| {
            t.lines()
                .nth(1)
                .and_then(|l| l.split_whitespace().next())
                .map(String::from)
        });
        if let (Some(device), Some(mounts)) = (device, run("mount", &[])) {
            for line in mounts.lines() {
                if line.starts_with(&format!("{device} on "))
                    && let Some(kind) = line
                        .rsplit_once('(')
                        .and_then(|(_, rest)| rest.split([',', ')']).next())
                {
                    return kind.trim().to_string();
                }
            }
        }
    }
    "unknown".to_string()
}
