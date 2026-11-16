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

export const getConfig = () => invoke<AppConfig>("get_config");
export const setMode = (mode: Mode, dataDir: string | null, serverUrl: string | null) =>
  invoke<AppConfig>("set_mode", { mode, dataDir, serverUrl });
export const setStartView = (startView: StartView) => invoke<AppConfig>("set_start_view", { startView });
export const startProcesses = () => invoke("start_processes");
export const stopProcesses = () => invoke("stop_processes");
export const processLogs = () => invoke<{ server: string[]; pipeline: string[] }>("process_logs");
export const mcpInfo = () => invoke<{ sidecar: string; dataDir: string }>("mcp_info");
export const openLink = (url: string) => invoke("open_link", { url });
export const getAutostart = () => invoke<boolean>("get_autostart");
export const setAutostart = (on: boolean) => invoke<boolean>("set_autostart", { on });
export const logOut = () => invoke("log_out");
export const modelState = () => invoke<ModelState>("model_state");
export const modelPull = () => invoke("model_pull");
export const modelPause = () => invoke<ModelState>("model_pause");
export const modelRemove = () => invoke<ModelState>("model_remove");

/** The text of a failed command: Rust sends a string. */
export function failure(error: unknown): string {
  return typeof error === "string" ? error : error instanceof Error ? error.message : String(error);
}
