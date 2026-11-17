import type { Mode, Status } from "../lib/api";
import { type BackupFolder, defaultBackupDir, type JobKind, type JobProgress, type SyncCheck } from "./api";
import type { StepState } from "./checklist";

// Step 5 of the setup: the backup of the Supernote, the import of its files and then of its
// app data, in this fixed order (the CLI refuses the app data before the files). The Setup
// page only shows what these functions say.

/** The import state of the account from GET /account (status.account). */
export type ImportState = NonNullable<Status["account"]>;

/** The dry run of `cloud import --json` (docs/reference.md, Import for apps). */
export interface DryRun {
  folders: { source: string; target: string; files: number; folders: number; bytes: number }[];
  skipped: { path: string; reason: string }[];
  errors: string[];
  filesPresent: number;
  files: number;
  bytes: number;
  needsReplace: boolean;
}

/** The result of `cloud import-app-data --json`. */
export interface AppData {
  dryRun: boolean;
  digests: number;
  groups: number;
  marks: number;
  leftToDevice: unknown[];
  withoutGroup: unknown[];
  skipped: unknown[];
  replaced: number;
}

/** How the last step ended: with its result line (json) or without (text only). */
export interface Ended {
  kind: JobKind;
  ok: boolean;
  json: Record<string, unknown> | null;
  text: string | null;
}

export type ImportStage =
  | "address"
  | "backingUp"
  | "paused"
  | "backupDone"
  | "tooOld"
  | "review"
  | "importing"
  | "snbak"
  | "appDataReview"
  | "appDataImporting"
  | "appDataFailed"
  | "done"
  | "failed";

export interface FlowInput {
  mode: Mode;
  account: ImportState | null;
  job: JobKind | null;
  nowMs: number;
  /** The last device sync of the account (status.device.lastSyncMs). */
  lastSyncMs: number | null;
  /** The backup folder in use and what it holds; null before one is chosen. */
  backup: { state: BackupFolder; madeThisRun: boolean } | null;
  /** The last files dry run of this backup. */
  dryRun: DryRun | null;
  /** The chosen .snbak file (or the empty choice): its file time and whether it was chosen
   * in this run. */
  snbak: { timeMs: number; chosenThisRun: boolean } | null;
  /** The last app data dry run that ended well. */
  appData: AppData | null;
  last: Ended | null;
  /** Import again: a new backup and a new .snbak, both imports with Replace. */
  again: boolean;
  /** Import again: the files, or the app data, were imported in this run. */
  filesThisRun: boolean;
  appDataThisRun: boolean;
}

const DAY = 24 * 3_600_000;

const RUNNING: Record<JobKind, ImportStage> = {
  backup: "backingUp",
  importDryRun: "review",
  import: "importing",
  appDataDryRun: "appDataReview",
  appData: "appDataImporting",
};

/** Where step 5 stands. The import state of the server comes first, then the running step,
 * then what this run did; so it holds again after the page is opened anew. */
export function importStage(input: FlowInput): ImportStage {
  const account = input.account;
  if (input.mode !== "local") return account?.imported === true ? "done" : "address";
  if (input.job !== null) return RUNNING[input.job];
  // What this run imported counts at once, also while the server starts again after it.
  const filesDone = input.again ? input.filesThisRun : account?.imported === true || input.filesThisRun;
  const appDataDone = input.again
    ? input.appDataThisRun
    : account?.appDataImported === true || input.appDataThisRun;
  if (filesDone && appDataDone) return "done";
  const last = input.last;
  if (filesDone) {
    if (last?.kind === "appData" && !last.ok) return "appDataFailed";
    const snbak = input.snbak;
    if (snbak === null || (input.again && !snbak.chosenThisRun)) return "snbak";
    if (snbakRefusal(snbak.timeMs, input.nowMs) !== null) return "snbak";
    if (last?.kind === "appDataDryRun" && !last.ok) return "snbak";
    return input.appData !== null ? "appDataReview" : "snbak";
  }
  if (last?.kind === "import" && !last.ok) return "failed";
  const backup = input.backup;
  if (backup === null || (input.again && !backup.madeThisRun)) return "address";
  const state = backup.state;
  if (state.state !== "complete" && state.state !== "resumable") return "address";
  const old = backupTooOld(Date.parse(state.time), input.nowMs, input.lastSyncMs);
  if (last?.kind === "backup" && !last.ok) {
    // Resume only within the 24 hours, and never over files the Supernote no longer has.
    return staleIn(last.json) || old ? "tooOld" : "backupDone";
  }
  switch (state.state) {
    case "resumable":
      return old ? "tooOld" : "paused";
    case "complete":
      return old ? "tooOld" : "review";
  }
}

/** A backup report (backup.json, snake_case) that lists files the Supernote no longer has:
 * the import refuses it, only a new backup helps. */
function staleIn(json: Record<string, unknown> | null): boolean {
  const stale = json?.stale;
  return Array.isArray(stale) && stale.length > 0;
}

/** Resume continues only a backup that did not finish: a run that ended with errors leaves a
 * finished folder, which takes no Resume (only a new backup into a new folder). */
export function backupResumable(state: BackupFolder): boolean {
  return state.state === "resumable";
}

/** A backup must not be imported when it is older than 24 hours or older than the last device
 * sync: a file missing on the server is deleted on the Supernote. A time that cannot be read is
 * too old as well. */
export function backupTooOld(backupTimeMs: number, nowMs: number, lastSyncMs: number | null): boolean {
  if (!Number.isFinite(backupTimeMs)) return true;
  if (nowMs - backupTimeMs > DAY) return true;
  return lastSyncMs !== null && backupTimeMs < lastSyncMs;
}

/** Why a backup is too old, with the only way on. */
export function tooOldLine(backupTimeMs: number, nowMs: number, lastSyncMs: number | null): string {
  const why =
    Number.isFinite(backupTimeMs) && nowMs - backupTimeMs <= DAY && lastSyncMs !== null && backupTimeMs < lastSyncMs
      ? "The Supernote synced after this backup."
      : "This backup is older than 24 hours.";
  return `${why} Make a new backup into a new folder.`;
}

/** Why a .snbak file is refused: older than 24 hours. null when it may be used. */
export function snbakRefusal(timeMs: number, nowMs: number): string | null {
  if (Number.isFinite(timeMs) && nowMs - timeMs <= DAY) return null;
  return "This .snbak file is older than 24 hours. Make a new one on the Supernote and copy it to this Mac.";
}

function megabytes(bytes: number): string {
  return `${(bytes / 1_000_000).toFixed(1)} MB`;
}

function files(n: number): string {
  return n === 1 ? "1 file" : `${n} files`;
}

const two = (n: number) => String(n).padStart(2, "0");
const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/** "Backup made today at 14:05.", by the local clock. */
export function backupTimeLine(timeMs: number, nowMs: number): string {
  if (!Number.isFinite(timeMs)) return "The time of the backup is not known.";
  const at = new Date(timeMs);
  const now = new Date(nowMs);
  const yesterday = new Date(now.getFullYear(), now.getMonth(), now.getDate() - 1);
  const sameDay = (a: Date, b: Date) =>
    a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate();
  const clock = `${two(at.getHours())}:${two(at.getMinutes())}`;
  const day = sameDay(at, now)
    ? "today"
    : sameDay(at, yesterday)
      ? "yesterday"
      : `on ${at.getDate()} ${MONTHS[at.getMonth()]} ${at.getFullYear()}`;
  return `Backup made ${day} at ${clock}.`;
}

/** The lines of the files review: the backup time, one line per device folder, what is
 * skipped, the errors, the files the server already holds and an empty Supernote. */
export function reviewLines(dryRun: DryRun, backupTimeMs: number, nowMs: number): string[] {
  const lines = [backupTimeLine(backupTimeMs, nowMs)];
  for (const folder of dryRun.folders) {
    lines.push(`${folder.source}: ${files(folder.files)}, ${megabytes(folder.bytes)}`);
  }
  for (const skip of dryRun.skipped) lines.push(`Skipped ${skip.path} (${skip.reason}).`);
  for (const error of dryRun.errors) lines.push(`Error: ${error}`);
  if (dryRun.needsReplace) {
    lines.push(
      `The server already holds ${files(dryRun.filesPresent)}. Replace moves them to the recycle bin first.`,
    );
  }
  if (dryRun.files === 0) lines.push("The Supernote is empty.");
  return lines;
}

/** What the files review offers: Import (not with errors), the Replace checkbox, and the empty
 * choice ("My Supernote is empty") for a backup without files. */
export function reviewActions(dryRun: DryRun): { canImport: boolean; replace: boolean; empty: boolean } {
  return { canImport: dryRun.errors.length === 0, replace: dryRun.needsReplace, empty: dryRun.files === 0 };
}

function count(n: number, one: string, many: string): string {
  return `${n} ${n === 1 ? one : many}`;
}

/** The lines of the app data review. */
export function appDataLines(appData: AppData): string[] {
  const lines: string[] = [];
  if (appData.digests === 0 && appData.groups === 0 && appData.marks === 0) {
    lines.push("No digests.");
  } else {
    lines.push(
      `${count(appData.digests, "digest", "digests")}, ${count(appData.groups, "group", "groups")} and ${count(appData.marks, "handwritten comment", "handwritten comments")}.`,
    );
  }
  const left = appData.leftToDevice.length;
  if (left > 0) lines.push(`${count(left, "record stays", "records stay")} on the Supernote.`);
  const without = appData.withoutGroup.length;
  if (without > 0) lines.push(`${count(without, "digest keeps", "digests keep")} no group: its group is not imported.`);
  const skipped = appData.skipped.length;
  if (skipped > 0) lines.push(`${count(skipped, "entry is", "entries are")} skipped: no imported digest names them.`);
  if (appData.replaced > 0) lines.push(`Replace clears ${appData.replaced} digest records on the server first.`);
  return lines;
}

/** What a failed files import says; the next try needs Replace (the server may hold some of
 * the files by now). */
export function importFailure(last: Ended): { text: string; replace: boolean } {
  const error = typeof last.json?.error === "string" ? last.json.error : last.text;
  return { text: `The import did not finish: ${error ?? "it stopped"}`, replace: true };
}

/** The text of a failed step: its error line, or its text. */
export function endedText(last: Ended): string | null {
  if (typeof last.json?.error === "string") return last.json.error;
  return last.text;
}

/** The import state on the Supernote page. */
export function importStatusText(account: ImportState | null): string {
  if (account === null) return "Not known yet.";
  if (account.imported === null || account.appDataImported === null) return "Not known. Update the server.";
  if (account.imported && account.appDataImported) return "Imported";
  if (account.imported) return "Files imported, digests not yet";
  return "Not imported yet";
}

/** "Import again" is offered under a done step 5 once a device of the account synced. */
export function importAgainOffered(mode: Mode, account: ImportState | null, lastSyncMs: number | null): boolean {
  return mode === "local" && account?.imported === true && account.appDataImported === true && lastSyncMs !== null;
}

/** The backup folder of a new backup: ~/supernote-backup-YYYY-MM-DD, then -2, -3 when taken. */
export function newBackupDir(today: Date, taken: number): string {
  const base = defaultBackupDir(today);
  return taken === 0 ? base : `${base}-${taken + 1}`;
}

/** The progress of a backup: no total is known up front. */
export function backupProgressText(n: number, bytes: number): string {
  return `${files(n)}, ${megabytes(bytes)} so far`;
}

/** The progress of an import by its phase; null for a line without one. */
export function importProgressText(progress: JobProgress): string | null {
  switch (progress.phase) {
    case "recycling":
      return `Moving ${files(progress.files ?? 0)} to the recycle bin.`;
    case "importing":
      return `Imported ${progress.done ?? 0} of ${files(progress.total ?? 0)}.`;
    case "verifying":
      return "Checking every file against the backup.";
    case "restarting":
      return "Starting the server again.";
    default:
      return null;
  }
}

/** Whether the import part of step 5 shows, and a line for it. A step blocked after an import
 * ended (the server starts again, or did not start) keeps its part with the last result, and
 * asks for Start while the server on this Mac neither runs nor starts (`server` is its state,
 * status.server.state.state). */
export function importPanel(input: {
  state: StepState;
  job: JobKind | null;
  last: Ended | null;
  mode: Mode;
  server: string | null;
}): { show: boolean; line: string | null } {
  if (input.state !== "blocked") return { show: true, line: null };
  if (input.job !== null) return { show: true, line: null };
  if (input.last === null) return { show: false, line: null };
  const up = input.server === "running" || input.server === "starting";
  return { show: true, line: input.mode === "local" && !up ? "Press Start in step 1." : null };
}

/** What step 6 shows after the first sync was checked: a line when nothing (or nothing more)
 * needs a look, else the lines of a banner with "Open the recycle bin" and "I have seen this".
 * A server on another machine sends no digest events, and the step says so. */
export function firstSyncView(
  checked: SyncCheck | null,
  acknowledged: boolean,
  mode: Mode,
): { line: string | null; banner: string[] | null } {
  if (checked === null) return { line: null, banner: null };
  const noDigests = mode === "remote" || checked.digests === null;
  const digestNote = "Deleted digests are not checked for a server on another machine.";
  const deletes = checked.files + checked.folders + (checked.digests ?? 0);
  if (!checked.cannotTell && deletes === 0) {
    const line = "The first sync worked. Nothing was deleted. Now turn Auto Sync on, on the Supernote.";
    return { line: noDigests ? `${line} ${digestNote}` : line, banner: null };
  }
  if (acknowledged) return { line: "Now turn Auto Sync on, on the Supernote.", banner: null };
  if (checked.cannotTell) {
    return { line: null, banner: ["Notedown cannot tell what the first sync changed. Check the recycle bin."] };
  }
  // The first count that is not 0 leads; the others follow.
  const counts: string[] = [];
  if (checked.files > 0) counts.push(files(checked.files));
  if (checked.folders > 0) counts.push(count(checked.folders, "folder", "folders"));
  if ((checked.digests ?? 0) > 0) counts.push(count(checked.digests ?? 0, "digest", "digests"));
  const banner = [`The Supernote deleted ${counts[0]} on the server. They are in the recycle bin.`];
  if (counts.length > 1) banner.push(`It also deleted ${counts.slice(1).join(" and ")}.`);
  if (noDigests) banner.push(digestNote);
  return { line: null, banner };
}
