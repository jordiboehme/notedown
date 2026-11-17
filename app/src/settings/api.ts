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
/** Polls the server and the pipeline now; the new status comes as an event. */
export const recheck = () => invoke<void>("recheck");
export const modelState = () => invoke<ModelState>("model_state");
export const modelPull = () => invoke("model_pull");
export const modelPause = () => invoke<ModelState>("model_pause");
export const modelRemove = () => invoke<ModelState>("model_remove");
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
