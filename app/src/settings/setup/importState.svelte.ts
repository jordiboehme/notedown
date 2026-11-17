import { listen } from "@tauri-apps/api/event";
import {
  appDataDryRun,
  appDataRun,
  type BackupFolder,
  backupFolder,
  backupPause,
  backupStart,
  failure,
  importDryRun,
  importRun,
  type JobFailed,
  type JobKind,
  type JobProgress,
  type JobResult,
  jobState,
  SETUP_JOB_DONE_EVENT,
  SETUP_JOB_FAILED_EVENT,
  SETUP_JOB_PAUSED_EVENT,
  SETUP_JOB_PROGRESS_EVENT,
  SETUP_RECORD_EVENT,
  setSetupRecord,
  snbakTime,
} from "../api";
import { config, loadConfig } from "../config.svelte";
import { type AppData, type DryRun, type Ended, newBackupDir, snbakRefusal } from "../importFlow";

// What step 5 knows of this run: the running step and its progress, how the last step ended,
// the last dry runs and the choices. It lives as long as the settings window, so a page change
// or a step that is blocked for a moment (the server starts again) loses nothing.

export interface FlowState {
  job: JobKind | null;
  progress: JobProgress | null;
  last: Ended | null;
  /** A command that was refused before a step could start. */
  error: string | null;
  dryRun: DryRun | null;
  appData: AppData | null;
  backup: { dir: string; state: BackupFolder; madeThisRun: boolean } | null;
  /** The .snbak file, or the empty choice (path null). */
  snbak: { path: string | null; timeMs: number; chosenThisRun: boolean } | null;
  /** Replace for the files import. */
  replace: boolean;
  /** Replace for the app data import. */
  appDataReplace: boolean;
  again: boolean;
  filesThisRun: boolean;
  appDataThisRun: boolean;
}

export const flow = $state<FlowState>({
  job: null,
  progress: null,
  last: null,
  error: null,
  dryRun: null,
  appData: null,
  backup: null,
  snbak: null,
  replace: false,
  appDataReplace: false,
  again: false,
  filesThisRun: false,
  appDataThisRun: false,
});

let started = false;

/** Follows the steps' events for the life of the window; once. */
export function startImportFlow(): void {
  if (started) return;
  started = true;
  void listen<JobProgress>(SETUP_JOB_PROGRESS_EVENT, (event) => {
    // The server starting again while no step runs here: a start that was refused (its command
    // already returned the error). It must not show the step as running.
    if (flow.job === null && event.payload.phase === "restarting") return;
    flow.job = event.payload.kind;
    flow.progress = event.payload;
  });
  void listen<JobResult>(SETUP_JOB_DONE_EVENT, (event) => {
    const { kind, ok, json } = event.payload;
    void ended({ kind, ok, json, text: null });
  });
  void listen<JobFailed>(SETUP_JOB_FAILED_EVENT, (event) => {
    void ended({ kind: event.payload.kind, ok: false, json: null, text: event.payload.text });
  });
  void listen(SETUP_JOB_PAUSED_EVENT, () => {
    flow.job = null;
    flow.progress = null;
    flow.last = null;
    void readBackup();
  });
  void listen(SETUP_RECORD_EVENT, () => void loadConfig());
  jobState()
    .then((kind) => (flow.job = kind))
    .catch(() => {});
}

/** Reads what the backup folder of the record (or of this run) and the .snbak hold. */
export async function readChoices(): Promise<void> {
  await Promise.all([readBackup(), readSnbak()]);
}

async function readBackup(dir?: string): Promise<void> {
  const path = dir ?? flow.backup?.dir ?? config.current?.setup.backupDir ?? null;
  if (path === null) return;
  try {
    const state = await backupFolder(path);
    const madeThisRun = flow.backup?.dir === path && flow.backup.madeThisRun;
    flow.backup = { dir: path, state, madeThisRun };
  } catch (error) {
    flow.error = failure(error);
  }
}

async function readSnbak(): Promise<void> {
  const path = config.current?.setup.snbak ?? null;
  if (path === null || flow.snbak !== null) return;
  try {
    flow.snbak = { path, timeMs: await snbakTime(path), chosenThisRun: false };
  } catch {
    // A file that is gone is chosen again.
  }
}

async function ended(end: Ended): Promise<void> {
  flow.job = null;
  flow.progress = null;
  flow.last = end;
  switch (end.kind) {
    case "backup":
      await readBackup();
      if (end.ok) await check();
      break;
    case "importDryRun":
      flow.dryRun = end.json !== null && Array.isArray(end.json.folders) ? (end.json as unknown as DryRun) : null;
      break;
    case "import":
      flow.filesThisRun = end.ok;
      break;
    case "appDataDryRun":
      flow.appData = end.ok && end.json !== null ? (end.json as unknown as AppData) : null;
      break;
    case "appData":
      flow.appDataThisRun = end.ok;
      break;
  }
  // Rust may have taken the mark or cleared the backup choices.
  void loadConfig();
}

/** Runs a command that starts a step; a refusal is shown in the step. */
async function run(kind: JobKind, start: () => Promise<unknown>): Promise<void> {
  flow.error = null;
  try {
    flow.job = kind;
    await start();
  } catch (error) {
    flow.job = null;
    flow.error = failure(error);
    // What runs after the refusal, from Rust: a late progress event cannot leave a step shown
    // as running.
    try {
      flow.job = await jobState();
    } catch {
      // The step shows the refusal; the next event sets the job.
    }
  }
}

/** Make a backup of the Supernote at `url` into `dir` (missing or empty). A folder with an
 * earlier backup is refused by Rust; its state is read so the step can offer what fits. */
export async function makeBackup(url: string, dir: string): Promise<void> {
  flow.error = null;
  try {
    config.current = await setSetupRecord({ backupUrl: url, backupDir: dir });
  } catch (error) {
    flow.error = failure(error);
    return;
  }
  // This run's backup from its start: a pause or an error keeps it this run's (Resume).
  flow.backup = { dir, state: { state: "empty" }, madeThisRun: true };
  flow.dryRun = null;
  flow.last = null;
  // A Replace ticked for an earlier backup does not carry over.
  flow.replace = false;
  await run("backup", () => backupStart(url, dir, false));
  if (flow.error !== null) {
    // Refused: what the folder holds is not this run's (an earlier backup there).
    flow.backup = null;
    await readBackup(dir);
  }
}

/** Continues the backup that did not finish, into the same folder. */
export async function resumeBackup(): Promise<void> {
  const url = config.current?.setup.backupUrl ?? null;
  const dir = flow.backup?.dir ?? null;
  if (url === null || dir === null) {
    flow.error = "Enter the address of the Supernote first.";
    return;
  }
  flow.last = null;
  await run("backup", () => backupStart(url, dir, true));
}

export async function pauseBackup(): Promise<void> {
  try {
    await backupPause();
  } catch (error) {
    flow.error = failure(error);
  }
}

/** Forgets the backup of this run: the next one goes into a new folder, which this finds. */
export async function newBackup(today: Date): Promise<string> {
  flow.backup = null;
  flow.replace = false;
  flow.dryRun = null;
  flow.last = null;
  flow.error = null;
  for (let taken = 0; taken < 20; taken += 1) {
    const dir = newBackupDir(today, taken);
    try {
      const state = await backupFolder(dir);
      if (state.state === "missing" || state.state === "empty") return dir;
    } catch {
      return dir;
    }
  }
  return newBackupDir(today, 20);
}

/** The dry run of the files import, always without Replace: then it says whether the server
 * holds files (needsReplace), and the review shows what Replace would move. */
export async function check(): Promise<void> {
  await run("importDryRun", () => importDryRun(false, false));
}

/** The files import; `allowEmpty` for a Supernote without files. */
export async function importFiles(allowEmpty: boolean, replace = flow.replace || flow.again): Promise<void> {
  flow.filesThisRun = false;
  flow.last = null;
  await run("import", () => importRun(replace, allowEmpty));
}

/** Uses the .snbak file at `path`: refused when older than 24 hours, else its dry run. */
export async function chooseSnbak(path: string): Promise<void> {
  flow.error = null;
  flow.appData = null;
  flow.last = null;
  try {
    config.current = await setSetupRecord({ snbak: path });
    flow.snbak = { path, timeMs: await snbakTime(path), chosenThisRun: true };
  } catch (error) {
    flow.error = failure(error);
    return;
  }
  if (snbakRefusal(flow.snbak.timeMs, Date.now()) !== null) return;
  await run("appDataDryRun", () => appDataDryRun(false, flow.appDataReplace || flow.again));
}

/** The empty choice: the Supernote has no digests. */
export async function noDigests(): Promise<void> {
  flow.appData = null;
  flow.last = null;
  flow.snbak = { path: null, timeMs: Date.now(), chosenThisRun: true };
  // An account that holds digests (Import again) needs Replace for the empty choice too.
  const replace = flow.appDataReplace || flow.again;
  await run("appDataDryRun", () => appDataDryRun(true, replace));
}

/** The app data import of the reviewed .snbak (or the empty choice). */
export async function importAppData(replace = flow.appDataReplace || flow.again): Promise<void> {
  const empty = flow.snbak?.path === null;
  flow.appDataThisRun = false;
  flow.last = null;
  await run("appData", () => appDataRun(empty, replace));
}

/** Back to the .snbak choice after a failed app data import. */
export function chooseAnotherSnbak(): void {
  flow.snbak = null;
  flow.appData = null;
  flow.last = null;
  flow.error = null;
}

/** Import again: a new backup and a new .snbak of this run, both imports with Replace. */
export function startAgain(): void {
  Object.assign(flow, {
    last: null,
    error: null,
    dryRun: null,
    appData: null,
    backup: null,
    snbak: null,
    replace: true,
    appDataReplace: true,
    again: true,
    filesThisRun: false,
    appDataThisRun: false,
  });
}
