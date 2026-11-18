import { invoke } from "@tauri-apps/api/core";

/** The event that carries a new {@link Status}. */
export const STATUS_EVENT = "status";

export type Mode = "notSet" | "local" | "remote";

export type ChildState =
  | { state: "stopped" }
  | { state: "notConnected" }
  | { state: "starting" }
  | { state: "running"; pid: number; sinceMs: number }
  | { state: "stopping" }
  | { state: "backoff"; untilMs: number; lastError: string }
  | { state: "outside"; what: string };

export type Attention =
  | "serverUnreachable"
  | "ocrError"
  | "serverBackoff"
  | "pipelineBackoff"
  | "tokenRefused"
  | "oldSync";

export interface InWork {
  note: string;
  path: string;
  page: number;
  pages: number;
  startedMs: number;
}

/** How many files a device sync changed; `files` is null when it is not known. */
export interface SyncCount {
  /** The lastSyncMs of the sync (the server's clock). */
  atMs: number;
  files: number | null;
  /** When the app stored the count (this Mac's clock); missing when it cannot tell. */
  countedAtMs?: number;
}

export interface LastError {
  text: string;
  atMs: number;
  /** What the failure is ("token_refused"); null from an older pipeline. */
  code: string | null;
}

export interface AgentStatus {
  name: string;
  /** The journal seq the consumer acked up to. */
  seq: number;
  readAtMs: number | null;
  ackedAtMs: number | null;
  notesBehind: number;
  more: boolean;
  /** "Up to date", "3 notes behind" or "40+ notes behind". */
  behind: string;
}

/** What the panel and the tray show; made by the poller in Rust. */
export interface Status {
  setup: { mode: Mode; ready: boolean; line: string | null; note: string | null };
  server: { line: string; state: ChildState | null; deviceUrls: string[] };
  pipeline: { line: string; state: ChildState };
  /** `line` is null when the device synced: say when, from `lastSyncMs`. */
  device: {
    line: string | null;
    name: string | null;
    lastSyncMs: number | null;
    syncing: boolean;
    online: boolean;
    /** The files the shown sync changed; null unless they were counted for that sync. */
    filesChanged: SyncCount | null;
  };
  ocr: {
    line: string;
    inWork: InWork | null;
    lastError: LastError | null;
    queued: number;
    pending: number;
    waiting: number;
    /** When the last pass ended. */
    lastRunMs: number | null;
    /** The status has an answer of `pipeline status`. */
    facts: boolean;
    /** The server refused the OCR's token (after a password change): it must connect again. */
    tokenRefused: boolean;
  };
  agents: AgentStatus[];
  attention: Attention[];
  existing: { server: boolean; pipeline: boolean };
  /** The import state from the last `GET /account`; null unless the server answered it. A
   * field is null when an older server does not send it. */
  account: { imported: boolean | null; appDataImported: boolean | null } | null;
  /** How the server answered the last `GET /account`. */
  connection: Connection;
  /** Why the last check of the first sync failed; null when it did not fail or is done. The
   * app tries it again every minute. */
  firstSyncError: string | null;
}

export type Connection = "unknown" | "reached" | "refused" | "unreachable" | "failed";

export function getStatus(): Promise<Status> {
  return invoke<Status>("get_status");
}
