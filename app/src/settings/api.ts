import { invoke } from "@tauri-apps/api/core";
import type { Mode } from "../lib/api";

// The commands of the settings window (app/src-tauri/src/commands.rs).

export type StartView = "tree" | "columns";

export interface AppConfig {
  version: number;
  mode: Mode;
  dataDir: string;
  serverUrl: string | null;
  account: string | null;
  email: string | null;
  startView: StartView;
  /** The Markdown folder of each account, by its slug. */
  markdownDirs: Record<string, string>;
  /** What the setup checklist keeps between two starts. */
  setup: SetupRecord;
}

/** What the setup checklist keeps (config.rs SetupRecord). */
export interface SetupRecord {
  /** The address of the Supernote's Browse & Access page. */
  backupUrl: string | null;
  /** The backup folder; cleared once both imports are done. */
  backupDir: string | null;
  /** The device's .snbak file; cleared once both imports are done. */
  snbak: string | null;
  setupShown: boolean;
  /** The first Local mode turned Start at login on once. */
  autostartOffered: boolean;
  /** The journal end when the import was done, by the token key of the account. */
  syncMark: { key: string; seq: number } | null;
  /** What the first sync after the mark deleted. */
  syncChecked: SyncCheck | null;
  /** The user saw the deletes of the first sync. */
  syncAcknowledged: boolean;
}

export interface SyncCheck {
  files: number;
  folders: number;
  /** null where digests cannot be checked (a server on another machine). */
  digests: number | null;
  /** The journal had a gap: what the first sync changed is not known. */
  cannotTell: boolean;
}

/** What set_setup_record may change; a field left out stays, null clears it. */
export interface SetupChange {
  backupUrl?: string | null;
  backupDir?: string | null;
  snbak?: string | null;
  syncAcknowledged?: boolean;
}

/** The steps of the setup that run the notedown program. */
export type JobKind = "backup" | "importDryRun" | "import" | "appDataDryRun" | "appData";

/** A progress line of a step: a backup counts files and bytes, an import names its phase
 * (recycling, importing, verifying, and "restarting" while the app's server starts again). */
export interface JobProgress {
  kind: JobKind;
  files: number | null;
  bytes: number | null;
  done: number | null;
  total: number | null;
  phase: string | null;
}

/** A step that printed its result. json is the result (backup.json for a backup, the dry run,
 * the import); when ok is false it also holds the error text as json.error. */
export interface JobResult {
  kind: JobKind;
  ok: boolean;
  json: Record<string, unknown> & { error?: string };
}

/** A step that ended without a result. */
export interface JobFailed {
  kind: JobKind;
  text: string;
}

/** What a backup folder holds. time is backup.json's time (RFC 3339). */
export type BackupFolder =
  | { state: "missing" }
  | { state: "empty" }
  | { state: "resumable"; time: string }
  | { state: "complete"; time: string }
  | { state: "foreign" }
  | { state: "unreadable" };

export const SETUP_JOB_PROGRESS_EVENT = "setup-job-progress";
export const SETUP_JOB_DONE_EVENT = "setup-job-done";
export const SETUP_JOB_FAILED_EVENT = "setup-job-failed";
/** Pause stopped the backup ({kind}); the next step may start. */
export const SETUP_JOB_PAUSED_EVENT = "setup-job-paused";

/** What the commands that touch the server answer while an import runs. */
export const WAIT_FOR_IMPORT = "Wait until the import is done.";

/** The backup folder of a backup made on `today`: ~/supernote-backup-YYYY-MM-DD (local date). */
export function defaultBackupDir(today: Date): string {
  const two = (n: number) => String(n).padStart(2, "0");
  return `~/supernote-backup-${today.getFullYear()}-${two(today.getMonth() + 1)}-${two(today.getDate())}`;
}

export interface Progress {
  file: string;
  done: number;
  total: number;
}

export interface ModelState {
  installed: boolean;
  sizeBytes: number;
  path: string | null;
  pulling: boolean;
  progress: Progress | null;
  problem: string | null;
}

export const MODEL_PROGRESS_EVENT = "model-progress";
export const MODEL_READY_EVENT = "model-ready";
export const MODEL_FAILED_EVENT = "model-failed";
export const SETTINGS_PAGE_EVENT = "settings-page";

/** The config after a change, and a sentence when an old login stayed in the Keychain. */
export interface ConfigAnswer {
  config: AppConfig;
  keychain: string | null;
}

/** A data folder on this Mac that holds a server, a pipeline or both. */
export interface ExistingFolder {
  path: string;
  /** The accounts of a server folder, by name. */
  server: { accounts: ExistingAccount[] } | null;
  /** The server a pipeline folder reads from; url is null when pipeline.toml names none. */
  pipeline: { url: string | null } | null;
}

export interface ExistingAccount {
  slug: string;
  email: string;
  /** The device backup is imported. */
  imported: boolean;
}

/** What Create account answers when the account was made but the login after it failed. */
export const ACCOUNT_MADE = "The account is made. Log in with it.";

/** The links to the instructions that open_link opens besides the About links. */
export const SETUP_LINKS = {
  gettingStarted: "https://github.com/jordiboehme/notedown/blob/main/docs/getting-started.md",
  otherMachine:
    "https://github.com/jordiboehme/notedown/blob/main/docs/getting-started.md#the-server-on-another-machine",
  pipelineOnMac: "https://github.com/jordiboehme/notedown/blob/main/docs/getting-started.md#a-pipeline-on-a-mac",
};

export const getConfig = () => invoke<AppConfig>("get_config");
export const setMode = (mode: Mode, dataDir: string | null, serverUrl: string | null) =>
  invoke<ConfigAnswer>("set_mode", { mode, dataDir, serverUrl });
export const setStartView = (startView: StartView) => invoke<AppConfig>("set_start_view", { startView });
export const startProcesses = () => invoke("start_processes");
export const stopProcesses = () => invoke("stop_processes");
export const processLogs = () => invoke<{ server: string[]; pipeline: string[] }>("process_logs");
export const mcpInfo = () => invoke<{ sidecar: string; dataDir: string }>("mcp_info");
export const openLink = (url: string) => invoke("open_link", { url });
export const getAutostart = () => invoke<boolean>("get_autostart");
export const setAutostart = (on: boolean) => invoke<boolean>("set_autostart", { on });
/** Logs in and keeps a write token in the Keychain; answers the config with the account. */
export const logIn = (email: string, password: string) => invoke<ConfigAnswer>("log_in", { email, password });
/** Whether the Keychain keeps a login for the account in the config. */
export const hasLogin = () => invoke<boolean>("has_login");
/** Deletes the login, forgets the account and asks the server to revoke the token (best
 * effort); answers the config without the account. */
export const logOut = () => invoke<AppConfig>("log_out");
/** Makes an account on this Mac's server, then logs in with it; answers the config with the
 * account. */
export const createAccount = (slug: string, email: string, password: string) =>
  invoke<ConfigAnswer>("create_account", { slug, email, password });
/** Connects this Mac's pipeline to the server on another machine with the config's e-mail. */
export const connectPipeline = (password: string) => invoke<ConfigAnswer>("connect_pipeline", { password });
/** What the config's data folder, ~/notedown-data and ~/notedown-mac hold. */
export const existingDetails = () => invoke<ExistingFolder[]>("existing_details");
/** The home folder of this Mac's user; null when the app has none. */
export const homeDir = () => invoke<string | null>("home_dir");
/** Polls the server and the pipeline now; the new status comes as an event. */
export const recheck = () => invoke<void>("recheck");
export const modelState = () => invoke<ModelState>("model_state");
export const modelPull = () => invoke("model_pull");
export const modelPause = () => invoke<ModelState>("model_pause");
export const modelRemove = () => invoke<ModelState>("model_remove");
/** Backs the Supernote at url up into dir; progress and the end come as events. */
export const backupStart = (url: string, dir: string, resume: boolean) =>
  invoke<void>("backup_start", { url, dir, resume });
/** Stops the running backup; Resume continues it once setup-job-paused came. */
export const backupPause = () => invoke<void>("backup_pause");
export const backupFolder = (dir: string) => invoke<BackupFolder>("backup_folder", { dir });
/** The dry run of the import of setup.backupDir. */
export const importDryRun = (replace: boolean, allowEmpty: boolean) =>
  invoke<void>("import_dry_run", { replace, allowEmpty });
/** Imports setup.backupDir with the app's server stopped. */
export const importRun = (replace: boolean, allowEmpty: boolean) =>
  invoke<void>("import_run", { replace, allowEmpty });
/** The dry run of the app data import of setup.snbak, or of nothing with empty. */
export const appDataDryRun = (empty: boolean, replace: boolean) =>
  invoke<void>("app_data_dry_run", { empty, replace });
/** Imports the app data with the app's server stopped. */
export const appDataRun = (empty: boolean, replace: boolean) => invoke<void>("app_data_run", { empty, replace });
/** The step that runs, if any. */
export const jobState = () => invoke<JobKind | null>("job_state");
export const setSetupRecord = (change: SetupChange) => invoke<AppConfig>("set_setup_record", { change });
/** When a .snbak file was last written, in ms since 1970. */
export const snbakTime = (path: string) => invoke<number>("snbak_time", { path });
/** The Markdown folder of the logged-in account; null without an account. */
export const markdownDir = () => invoke<string | null>("markdown_dir");
/** Saves the Markdown folder of the logged-in account; the pipeline starts again with it. */
export const setMarkdownDir = (dir: string) => invoke<string | null>("set_markdown_dir", { dir });
/** Opens the Markdown folder in the Finder (it is made first when it is not there yet). */
export const showMarkdownDir = () => invoke("show_markdown_dir");

/** The text of a failed command: Rust sends a string. */
export function failure(error: unknown): string {
  return typeof error === "string" ? error : error instanceof Error ? error.message : String(error);
}
