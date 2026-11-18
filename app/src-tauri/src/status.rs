//! What the panel and the tray show: the server, the pipeline, the Supernote, the OCR and the
//! agents, in short lines, and the problems that light the dot of the tray icon.
//! [`status_from`] is pure: the poller gathers the [`Parts`] and calls it.

use serde::{Deserialize, Serialize};

use crate::config::{AppConfig, ExistingSetup, Mode};
use crate::notify::store::SyncCount;
use crate::supervisor::ChildState;

/// The line of a child that another Notedown process replaces.
pub const OUTSIDE_LINE: &str = "Notedown runs outside the app. Stop it there, then press Start.";

/// The server must not answer for longer than this before it is a problem (2 minutes).
pub const UNREACHABLE_AFTER_MS: u64 = 2 * 60_000;

/// No sync of the Supernote for longer than this is a problem (7 days).
pub const OLD_SYNC_AFTER_MS: u64 = 7 * 24 * 3_600_000;

/// The note when the config file was broken and the app started with defaults.
pub const RESET_NOTE: &str =
    "The settings file could not be read. It was set aside and the settings were reset.";

/// What `pipeline status` says when the data folder has no account yet.
pub const NO_ACCOUNT: &str = "has no account";

/// The answer of `notedown pipeline status` (docs/reference.md, "Pipeline status"). Only the
/// fields the app shows; a missing field reads as its default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PipelineFacts {
    pub account: String,
    pub running: bool,
    pub pending: u64,
    pub queued: u64,
    pub waiting: u64,
    pub last_error: Option<LastError>,
    pub last_run_ms: Option<u64>,
    pub in_work: Option<InWork>,
    pub consumers: Vec<Consumer>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LastError {
    pub text: String,
    pub at_ms: u64,
    /// What the failure is: [`TOKEN_REFUSED`] or none. An older pipeline sends none.
    pub code: Option<String>,
}

/// The code of a last error when the server refused the pipeline's token: a new
/// `connect` fixes it.
pub const TOKEN_REFUSED: &str = "token_refused";

/// The page the pipeline reads now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InWork {
    pub note: String,
    pub path: String,
    pub page: u64,
    pub pages: u64,
    pub started_ms: u64,
}

/// A consumer of the changes, such as an agent over MCP.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Consumer {
    pub name: String,
    pub seq: u64,
    pub acked_at: Option<u64>,
    pub read_at: Option<u64>,
    pub notes_behind: u64,
    pub more: bool,
}

/// The result of one `pipeline status` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineCall {
    Facts(PipelineFacts),
    /// The call failed; the text is what it said on stderr, or how it ended.
    Failed(String),
}

/// The answer of `GET /rest/v1/account` (docs/rest-api.md, "Account").
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AccountInfo {
    pub slug: String,
    pub user_id: u64,
    /// An older server sends no list; that is an empty one.
    pub devices: Vec<Device>,
    /// Whether the Supernote's files were imported; `None` from an older server.
    pub imported: Option<bool>,
    /// Whether the Supernote's app data was imported; `None` from an older server.
    pub app_data_imported: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Device {
    pub serial: String,
    pub name: Option<String>,
    pub bound: bool,
    pub last_login_ms: Option<u64>,
    pub last_sync_ms: Option<u64>,
    pub syncing: bool,
    pub online: bool,
}

/// How the server answered `GET /account`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountAnswer {
    Account(AccountInfo),
    /// 401: no token, or the token was refused.
    Refused,
    /// No answer at all: no connection, or a timeout.
    Unreachable(String),
    /// Any other answer; the server is there.
    Failed(String),
}

/// The last `/account` poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountPoll {
    /// A token was stored and sent.
    pub had_token: bool,
    pub answer: AccountAnswer,
    /// Since when the server has not answered, over all polls in a row.
    pub unreachable_since_ms: Option<u64>,
}

/// Everything [`status_from`] needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parts {
    pub config: AppConfig,
    /// The config file was broken and was set aside.
    pub config_reset: bool,
    pub existing: ExistingSetup,
    pub server: ChildState,
    pub pipeline: ChildState,
    /// The addresses the Supernote uses, from the server's start lines.
    pub device_urls: Vec<String>,
    /// `None` until the first call ended.
    pub pipeline_status: Option<PipelineCall>,
    /// `None` until the first poll ended.
    pub account: Option<AccountPoll>,
    /// The count of the files of the last device sync the poller counted.
    pub sync_count: Option<SyncCount>,
}

/// A problem that lights the dot of the tray icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Attention {
    /// The server has not answered for more than 2 minutes.
    ServerUnreachable,
    /// The pipeline's last pass failed.
    OcrError,
    /// The server ended with an error and starts again later.
    ServerBackoff,
    /// The pipeline ended with an error and starts again later.
    PipelineBackoff,
    /// The server refused the stored token.
    TokenRefused,
    /// The Supernote has not synced for more than 7 days.
    OldSync,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub setup: Setup,
    pub server: ServerStatus,
    pub pipeline: PipelineStatus,
    pub device: DeviceStatus,
    pub ocr: OcrStatus,
    pub agents: Vec<AgentStatus>,
    pub attention: Vec<Attention>,
    pub existing: ExistingSetup,
    /// The import state of the account; only when the last `/account` poll answered it.
    pub account: Option<ImportState>,
    /// How the server answered the last `/account` poll.
    pub connection: Connection,
    /// Why the last check of the first sync of the login failed; `None` when it did not fail
    /// or the sync is checked. The poller fills it in and tries again every minute.
    pub first_sync_error: Option<String>,
}

/// What `GET /account` says about the import; `None` where an older server says nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportState {
    pub imported: Option<bool>,
    pub app_data_imported: Option<bool>,
}

/// How the server answered the last `/account` poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Connection {
    /// No poll yet (or none for this config).
    Unknown,
    /// The server answered the account.
    Reached,
    /// 401: no token, or the token was refused.
    Refused,
    /// No answer at all.
    Unreachable,
    /// Any other answer.
    Failed,
}

/// The connection and the import state from the last `/account` poll.
fn account_state(poll: Option<&AccountPoll>) -> (Connection, Option<ImportState>) {
    match poll.map(|p| &p.answer) {
        None => (Connection::Unknown, None),
        Some(AccountAnswer::Account(info)) => (
            Connection::Reached,
            Some(ImportState {
                imported: info.imported,
                app_data_imported: info.app_data_imported,
            }),
        ),
        Some(AccountAnswer::Refused) => (Connection::Refused, None),
        Some(AccountAnswer::Unreachable(_)) => (Connection::Unreachable, None),
        Some(AccountAnswer::Failed(_)) => (Connection::Failed, None),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Setup {
    pub mode: Mode,
    /// A mode is set and the data folder has an account.
    pub ready: bool,
    /// What is missing, when something is.
    pub line: Option<String>,
    /// The note that the config was reset.
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    pub line: String,
    /// The state of the app's own server; `None` in Remote mode and without a mode.
    pub state: Option<ChildState>,
    pub device_urls: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineStatus {
    pub line: String,
    pub state: ChildState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceStatus {
    /// `None` when the device synced: the webview says when, from `lastSyncMs`.
    pub line: Option<String>,
    pub name: Option<String>,
    pub last_sync_ms: Option<u64>,
    pub syncing: bool,
    pub online: bool,
    /// The files the shown sync changed; only when they were counted for that sync.
    pub files_changed: Option<SyncCount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrStatus {
    pub line: String,
    pub in_work: Option<InWork>,
    pub last_error: Option<LastError>,
    pub queued: u64,
    pub pending: u64,
    pub waiting: u64,
    /// When the last pass ended.
    pub last_run_ms: Option<u64>,
    /// This status has an answer of `pipeline status`; without it the fields above say
    /// nothing about the pipeline.
    pub facts: bool,
    /// The server refused the pipeline's token: the OCR must connect again. Only from the
    /// code of the last error, never from its text.
    pub token_refused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStatus {
    pub name: String,
    /// The journal seq the consumer acked up to.
    pub seq: u64,
    pub read_at_ms: Option<u64>,
    pub acked_at_ms: Option<u64>,
    pub notes_behind: u64,
    pub more: bool,
    /// "Up to date", "3 notes behind" or "40+ notes behind".
    pub behind: String,
}

/// The status as of `now_ms`.
pub fn status_from(parts: &Parts, now_ms: u64) -> Status {
    let config = &parts.config;
    let note = parts.config_reset.then(|| RESET_NOTE.to_owned());
    if config.mode == Mode::NotSet {
        return not_set_up(parts, note);
    }
    let mut attention = Vec::new();

    // No account in the data folder yet: a setup step, not an error.
    let no_account = config.account.as_deref().is_none_or(str::is_empty)
        && matches!(&parts.pipeline_status, Some(PipelineCall::Failed(e)) if e.contains(NO_ACCOUNT));
    let setup = Setup {
        mode: config.mode,
        ready: !no_account,
        line: no_account.then(|| "Notedown has no account yet".to_owned()),
        note,
    };

    let reach = reach(parts, now_ms);
    if reach == (Reach::Silent { long: true }) {
        attention.push(Attention::ServerUnreachable);
    }
    let server = server_status(parts, reach, &mut attention);
    let pipeline = PipelineStatus {
        line: child_line(&parts.pipeline, "Runs"),
        state: parts.pipeline.clone(),
    };
    if matches!(parts.pipeline, ChildState::Backoff { .. }) {
        attention.push(Attention::PipelineBackoff);
    }
    let device = device_status(parts, now_ms, &mut attention);
    let (ocr, agents) = ocr_status(parts, no_account, &mut attention);
    let (connection, account) = account_state(parts.account.as_ref());

    Status {
        setup,
        server,
        pipeline,
        device,
        ocr,
        agents,
        attention,
        existing: parts.existing,
        account,
        connection,
        first_sync_error: None,
    }
}

const NOT_SET_UP: &str = "Not set up yet";

fn not_set_up(parts: &Parts, note: Option<String>) -> Status {
    let (connection, account) = account_state(parts.account.as_ref());
    Status {
        setup: Setup {
            mode: Mode::NotSet,
            ready: false,
            line: Some("Notedown is not set up yet".to_owned()),
            note,
        },
        server: ServerStatus {
            line: NOT_SET_UP.to_owned(),
            state: None,
            device_urls: Vec::new(),
        },
        pipeline: PipelineStatus {
            line: NOT_SET_UP.to_owned(),
            state: parts.pipeline.clone(),
        },
        device: device_line(NOT_SET_UP),
        ocr: ocr_line(NOT_SET_UP),
        agents: Vec::new(),
        attention: Vec::new(),
        existing: parts.existing,
        account,
        connection,
        first_sync_error: None,
    }
}

/// Whether the server answers, as far as the app expects it to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// No answer yet, or the server is not expected to answer (stopped by the user).
    Unknown,
    Answers,
    /// No answer; `long` when for more than [`UNREACHABLE_AFTER_MS`].
    Silent {
        long: bool,
    },
}

fn reach(parts: &Parts, now_ms: u64) -> Reach {
    // In Local mode only a server that runs (ours or another Notedown) should answer, and
    // only since it started.
    let started = match (parts.config.mode, &parts.server) {
        (Mode::Local, ChildState::Running { since_ms, .. }) => Some(*since_ms),
        (Mode::Local, ChildState::Outside { .. }) | (Mode::Remote, _) => Some(0),
        _ => None,
    };
    let (Some(started), Some(poll)) = (started, &parts.account) else {
        return Reach::Unknown;
    };
    match poll.answer {
        AccountAnswer::Unreachable(_) => {
            let since = poll.unreachable_since_ms.unwrap_or(now_ms).max(started);
            Reach::Silent {
                long: now_ms.saturating_sub(since) > UNREACHABLE_AFTER_MS,
            }
        }
        _ => Reach::Answers,
    }
}

fn server_status(parts: &Parts, reach: Reach, attention: &mut Vec<Attention>) -> ServerStatus {
    let device_urls = parts.device_urls.clone();
    if parts.config.mode == Mode::Remote {
        // Never a login of the address in a line.
        let url = parts
            .config
            .server_url
            .as_deref()
            .filter(|u| !u.is_empty())
            .map(crate::config::without_userinfo);
        let line = match url {
            None => "No server address yet".to_owned(),
            Some(url) => match reach {
                Reach::Unknown => format!("Checking {url}..."),
                Reach::Answers => format!("Runs on {url}"),
                Reach::Silent { .. } => format!("{url} does not answer"),
            },
        };
        return ServerStatus {
            line,
            state: None,
            device_urls,
        };
    }
    if matches!(parts.server, ChildState::Backoff { .. }) {
        attention.push(Attention::ServerBackoff);
    }
    let line = match (&parts.server, reach) {
        (ChildState::Running { .. }, Reach::Silent { long: true }) => {
            "Runs on this Mac but does not answer".to_owned()
        }
        (state, _) => child_line(state, "Runs on this Mac"),
    };
    ServerStatus {
        line,
        state: Some(parts.server.clone()),
        device_urls,
    }
}

/// The line of a child's state; `running` is the line while it runs.
fn child_line(state: &ChildState, running: &str) -> String {
    match state {
        ChildState::Stopped => "Stopped".to_owned(),
        ChildState::NotConnected => "Not connected yet".to_owned(),
        ChildState::Starting => "Starting...".to_owned(),
        ChildState::Running { .. } => running.to_owned(),
        ChildState::Stopping => "Stopping...".to_owned(),
        ChildState::Backoff { last_error, .. } => {
            // The sentence gets one period: an error that ends with one loses it here.
            let error = last_error.strip_suffix('.').unwrap_or(last_error);
            format!("Stopped with an error: {error}. It starts again soon.")
        }
        ChildState::Outside { .. } => OUTSIDE_LINE.to_owned(),
    }
}

fn device_line(line: &str) -> DeviceStatus {
    DeviceStatus {
        line: Some(line.to_owned()),
        name: None,
        last_sync_ms: None,
        syncing: false,
        online: false,
        files_changed: None,
    }
}

fn device_status(parts: &Parts, now_ms: u64, attention: &mut Vec<Attention>) -> DeviceStatus {
    let server_up = matches!(
        parts.server,
        ChildState::Running { .. } | ChildState::Outside { .. }
    );
    if parts.config.mode == Mode::Local && !server_up {
        return device_line("Waiting for the server");
    }
    let Some(poll) = &parts.account else {
        return device_line("Checking...");
    };
    let info = match &poll.answer {
        AccountAnswer::Unreachable(_) => return device_line("The server does not answer"),
        AccountAnswer::Refused if poll.had_token => {
            attention.push(Attention::TokenRefused);
            return device_line("The server refused the login");
        }
        AccountAnswer::Refused => return device_line("Not logged in yet"),
        AccountAnswer::Failed(why) => {
            return device_line(&format!("The device status is not available: {why}"));
        }
        AccountAnswer::Account(info) => info,
    };
    // The newest sync, whatever the order of the list; a device without one comes last.
    let Some(device) = info.devices.iter().max_by_key(|d| d.last_sync_ms) else {
        return device_line("No Supernote yet");
    };
    let line = match device.last_sync_ms {
        None => Some("Not synced yet".to_owned()),
        Some(at) => {
            if now_ms.saturating_sub(at) > OLD_SYNC_AFTER_MS {
                attention.push(Attention::OldSync);
            }
            None
        }
    };
    DeviceStatus {
        line,
        name: device.name.clone(),
        last_sync_ms: device.last_sync_ms,
        syncing: device.syncing,
        online: device.online,
        files_changed: parts
            .sync_count
            .filter(|c| device.last_sync_ms == Some(c.at_ms)),
    }
}

fn ocr_line(line: &str) -> OcrStatus {
    OcrStatus {
        line: line.to_owned(),
        in_work: None,
        last_error: None,
        queued: 0,
        pending: 0,
        waiting: 0,
        last_run_ms: None,
        facts: false,
        token_refused: false,
    }
}

fn ocr_status(
    parts: &Parts,
    no_account: bool,
    attention: &mut Vec<Attention>,
) -> (OcrStatus, Vec<AgentStatus>) {
    if parts.pipeline == ChildState::NotConnected {
        return (ocr_line("Not connected yet"), Vec::new());
    }
    let facts = match &parts.pipeline_status {
        None => return (ocr_line("Checking..."), Vec::new()),
        Some(PipelineCall::Failed(_)) if no_account => {
            return (ocr_line("OCR starts when there is an account"), Vec::new());
        }
        Some(PipelineCall::Failed(why)) => {
            return (
                ocr_line(&format!("OCR status is not available: {why}")),
                Vec::new(),
            );
        }
        Some(PipelineCall::Facts(facts)) => facts,
    };
    let in_queue = facts.queued + facts.pending;
    let line = if let Some(e) = &facts.last_error {
        attention.push(Attention::OcrError);
        format!("The last OCR pass failed: {}", e.text)
    } else if in_queue > 0 {
        format!("{} waiting", count(in_queue, "page", "pages"))
    } else if facts.waiting > 0 {
        let verb = if facts.waiting == 1 { "waits" } else { "wait" };
        format!(
            "{} {verb} for the note to settle",
            count(facts.waiting, "page", "pages")
        )
    } else {
        "Caught up".to_owned()
    };
    let ocr = OcrStatus {
        line,
        in_work: facts.in_work.clone(),
        last_error: facts.last_error.clone(),
        queued: facts.queued,
        pending: facts.pending,
        waiting: facts.waiting,
        last_run_ms: facts.last_run_ms,
        facts: true,
        token_refused: facts
            .last_error
            .as_ref()
            .is_some_and(|e| e.code.as_deref() == Some(TOKEN_REFUSED)),
    };
    let agents = facts.consumers.iter().map(agent).collect();
    (ocr, agents)
}

fn agent(c: &Consumer) -> AgentStatus {
    let behind = match (c.notes_behind, c.more) {
        (0, false) => "Up to date".to_owned(),
        (n, true) => format!("{n}+ notes behind"),
        (n, false) => format!("{} behind", count(n, "note", "notes")),
    };
    AgentStatus {
        name: c.name.clone(),
        seq: c.seq,
        read_at_ms: c.read_at,
        acked_at_ms: c.acked_at,
        notes_behind: c.notes_behind,
        more: c.more,
        behind,
    }
}

fn count(n: u64, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

#[cfg(test)]
mod tests;
