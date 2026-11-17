import type { Status } from "../lib/api";
import type { AppConfig, ExistingFolder, JobKind, ModelState } from "./api";
import { NO_MODE, useFolder } from "./account";
import { slugFromEmail } from "./slug";

// The setup checklist: the state of each of its seven steps, derived from the status, the
// config, the model and the running step. The Setup page only shows what this says.

export type StepId = "server" | "account" | "connection" | "model" | "import" | "supernote" | "agent";
export type StepState = "done" | "todo" | "blocked" | "working" | "optional";

export interface Step {
  id: StepId;
  title: string;
  state: StepState;
  line: string;
  /** The first earlier step that is not done yet, when this one waits for it. */
  blockedBy?: StepId;
}

export interface ChecklistInput {
  status: Status;
  config: AppConfig;
  /** null while the model state is not read yet. */
  model: ModelState | null;
  existing: Status["existing"];
  details: ExistingFolder[];
  /** The step of the backup and the import that runs now. */
  job: JobKind | null;
  /** The Keychain keeps a login for the account of the config. */
  hasLogin: boolean;
  /** The last dry run of the files import, when one ran: it says whether the server already
   * holds files. */
  dryRun?: { filesPresent: number } | null;
  /** Import again runs and has not imported both the files and the digests yet: step 5 is not
   * done, though the server still says imported, and step 6 waits for it. */
  againRuns?: boolean;
}

/** What the server line says while Notedown runs outside the app (status.rs OUTSIDE_LINE). */
export const OUTSIDE_LINE = "Notedown runs outside the app. Stop it there, then press Start.";

export const TITLES: Record<StepId, string> = {
  server: "Choose the server",
  account: "Log in",
  connection: "Check the connection",
  model: "Download the OCR model",
  import: "Back up and import the Supernote",
  supernote: "Connect the Supernote",
  agent: "Connect an agent",
};

/** The steps in their order. A step of the chain needs every earlier step of the chain; the
 * OCR model and the agent need none and hold none up (the import does not need the OCR). */
const ORDER: StepId[] = ["server", "account", "connection", "model", "import", "supernote", "agent"];
const CHAIN: StepId[] = ["server", "account", "connection", "import", "supernote"];

/** The steps of the backup and the import that hold the app's server (it is stopped). */
const HOLDS_SERVER: JobKind[] = ["import", "appData"];

interface Own {
  done: boolean;
  line: string;
  working?: string;
}

export function checklist(input: ChecklistInput): Step[] {
  const own: Record<StepId, Own> = {
    server: server(input),
    account: account(input),
    connection: connection(input),
    model: model(input),
    import: importStep(input),
    supernote: supernote(input),
    agent: agent(input),
  };
  const steps: Step[] = [];
  for (const id of ORDER) {
    const step = own[id];
    const title = TITLES[id];
    if (step.working !== undefined) {
      steps.push({ id, title, state: "working", line: step.working });
      continue;
    }
    if (id === "agent") {
      steps.push({ id, title, state: step.done ? "done" : "optional", line: step.line });
      continue;
    }
    const chained = CHAIN.indexOf(id);
    // By the state shown: a step that works again (an import that runs) is not done.
    const shown = (earlier: StepId) => steps.find((s) => s.id === earlier)?.state;
    const before = chained === -1 ? undefined : CHAIN.slice(0, chained).find((earlier) => shown(earlier) !== "done");
    if (before !== undefined) {
      const line = `Do step ${ORDER.indexOf(before) + 1} first.`;
      steps.push({ id, title, state: "blocked", line, blockedBy: before });
      continue;
    }
    steps.push({ id, title, state: step.done ? "done" : "todo", line: step.line });
  }
  return steps;
}

function holdsServer(job: JobKind | null): boolean {
  return job !== null && HOLDS_SERVER.includes(job);
}

function server({ status, config, job }: ChecklistInput): Own {
  if (config.mode === "notSet") return { done: false, line: "Choose where the server runs." };
  if (config.mode === "local") {
    const state = status.server.state;
    if (holdsServer(job)) return { done: true, line: "The import holds the server on this Mac." };
    if (state === null) return { done: false, line: "Checking the server." };
    if (state.state === "outside") return { done: false, line: OUTSIDE_LINE };
    switch (state.state) {
      case "running":
        return { done: true, line: "The server runs on this Mac." };
      case "starting":
        return { done: false, line: "The server is starting." };
      case "stopping":
        return { done: false, line: "The server is stopping." };
      case "backoff":
        return { done: false, line: "The server stopped with an error. Press Start." };
      default:
        return { done: false, line: "The server is stopped. Press Start." };
    }
  }
  const where = config.serverUrl ? withoutUserinfo(config.serverUrl) : "the other machine";
  if (status.connection === "reached" || status.connection === "refused") {
    return { done: true, line: `The server at ${where} answers.` };
  }
  if (status.connection === "unknown") return { done: false, line: "Checking the server." };
  return { done: false, line: "The server does not answer." };
}

function account({ config, hasLogin }: ChecklistInput): Own {
  if (hasLogin && config.account !== null) return { done: true, line: `Logged in as ${config.account}.` };
  if (hasLogin) return { done: true, line: "Logged in." };
  return {
    done: false,
    line: config.mode === "local" ? "Log in, or create an account." : "Log in with your account on the server.",
  };
}

function connection({ status, config, existing, job }: ChecklistInput): Own {
  // The import stopped the server for its run; the connection worked before it could start.
  if (config.mode === "local" && holdsServer(job)) return { done: true, line: "The import holds the server." };
  switch (status.connection) {
    case "unknown":
      return { done: false, line: "Checking the connection." };
    case "unreachable":
      return { done: false, line: "The server does not answer." };
    case "failed":
      return { done: false, line: "The server answered with an error. Check again." };
    case "refused":
      return { done: false, line: "The server refused the login. Log in again." };
    case "reached":
      break;
  }
  if (status.account === null) return { done: false, line: "Checking the connection." };
  if (config.mode === "remote" && !existing.pipeline) {
    return { done: false, line: "The OCR is not connected to the server yet." };
  }
  return { done: true, line: "The server answers, and the login works." };
}

function model({ model }: ChecklistInput): Own {
  if (model === null) return { done: false, line: "Checking the OCR model." };
  if (model.pulling) return { done: false, line: "", working: "Downloading the OCR model." };
  if (model.installed) return { done: true, line: "The OCR model is on this Mac." };
  if (model.sizeBytes > 0) return { done: false, line: "The OCR model is partly downloaded. Download the rest." };
  return { done: false, line: "Download the OCR model. It is about 3.4 GB." };
}

const JOB_LINES: Record<JobKind, string> = {
  backup: "Backing up the Supernote.",
  importDryRun: "Checking the backup.",
  import: "Importing the files.",
  appDataDryRun: "Checking the digests.",
  appData: "Importing the digests.",
};

function importStep({ status, config, job, dryRun, againRuns }: ChecklistInput): Own {
  const working = job !== null ? JOB_LINES[job] : undefined;
  // The server still says imported from the first time: Import again is done only with both.
  if (againRuns === true) {
    return { done: false, line: "Import again is not finished. Import the files and the digests.", working };
  }
  const state = status.account;
  if (state === null) return { done: false, line: "Notedown cannot see the import yet.", working };
  if (state.imported === null) return { done: false, line: "Update the server on the other machine.", working };
  if (!state.imported) {
    if (dryRun && dryRun.filesPresent > 0) {
      return { done: false, line: "The last import did not finish. Import again with Replace.", working };
    }
    const line =
      config.mode === "remote"
        ? "Import the Supernote on the other machine."
        : "Back up the Supernote and import it.";
    return { done: false, line, working };
  }
  if (config.mode === "remote") return { done: true, line: "The Supernote is imported.", working };
  if (state.appDataImported === null) {
    return { done: false, line: "Update the server on the other machine.", working };
  }
  if (!state.appDataImported) {
    return { done: false, line: "The files are imported. Import the digests next.", working };
  }
  return { done: true, line: "The Supernote is imported.", working };
}

function supernote({ status, config }: ChecklistInput): Own {
  const record = config.setup;
  const mark = record.syncMark;
  const own = mark !== null && mark.key === tokenKey(config);
  if (!own) {
    // Another machine: the poll that first sees the import takes the mark, and only a sync
    // after it counts.
    if (config.mode !== "local") return { done: false, line: "Connect the Supernote and sync it once." };
    // A setup imported without the app (no mark of this login): the first sync is enough.
    return status.device.lastSyncMs !== null
      ? { done: true, line: ADOPTED_LINE }
      : { done: false, line: "Connect the Supernote and sync it once." };
  }
  const checked = record.syncChecked;
  if (checked === null) {
    const why = status.firstSyncError?.trim().replace(/\.+$/, "") ?? "";
    if (why !== "") {
      return {
        done: false,
        line: `Notedown could not check the first sync: ${why}. It tries again every minute.`,
      };
    }
    return { done: false, line: "Connect the Supernote and sync it once." };
  }
  const deletes = checked.files + checked.folders + (checked.digests ?? 0);
  if (!checked.cannotTell && deletes === 0) {
    return { done: true, line: "The first sync worked. Nothing was deleted." };
  }
  if (record.syncAcknowledged) return { done: true, line: "The first sync is done." };
  return {
    done: false,
    line: checked.cannotTell
      ? "Notedown cannot tell what the first sync changed. Check the recycle bin."
      : "The first sync deleted files on the server. Look at the recycle bin.",
  };
}

/** Step 6 of a setup imported without the app: no mark, so no check. */
export const ADOPTED_LINE =
  "Notedown did not check this first sync for deletes, because the import was done outside the app.";

function agent({ status }: ChecklistInput): Own {
  const names = status.agents.map((a) => a.name);
  if (names.length === 0) return { done: false, line: "Optional. Give an agent the notes over MCP." };
  return { done: true, line: names.length === 1 ? `${names[0]} reads the notes.` : `${names.length} agents read the notes.` };
}

/** The key Rust keeps the login and the sync mark by (token.rs token_key): the server and the
 * account. null without both. */
export function tokenKey(config: AppConfig): string | null {
  const slug = config.account;
  if (slug === null || slug === "") return null;
  if (config.mode === "local") return `local|${slug}`;
  if (config.mode === "remote") {
    const url = config.serverUrl;
    if (url === null || url === "") return null;
    return `${withoutUserinfo(url)}|${slug}`;
  }
  return null;
}

/** The address without a login in it (config.rs without_userinfo). */
function withoutUserinfo(url: string): string {
  const split = url.indexOf("://");
  if (split === -1) return url;
  const scheme = url.slice(0, split);
  const rest = url.slice(split + 3);
  const ends = rest.search(/[/?#]/);
  const end = ends === -1 ? rest.length : ends;
  const authority = rest.slice(0, end);
  const at = authority.lastIndexOf("@");
  return at === -1 ? url : `${scheme}://${authority.slice(at + 1)}${rest.slice(end)}`;
}

/** The dot and the word of a step's state. */
export function stepDot(state: StepState): { level: "good" | "warning" | "bad" | "unknown"; text: string } {
  switch (state) {
    case "done":
      return { level: "good", text: "Done" };
    case "todo":
      return { level: "warning", text: "To do" };
    case "blocked":
      return { level: "unknown", text: "Waiting" };
    case "working":
      return { level: "warning", text: "Working" };
    case "optional":
      return { level: "unknown", text: "Optional" };
  }
}

/** A path with the home folder (from the app; null while not known) as ~. */
export function shortPath(path: string, home: string | null): string {
  if (home === null || home === "" || home === "/") return path;
  const base = home.replace(/\/+$/, "");
  if (path === base) return "~";
  return path.startsWith(`${base}/`) ? `~${path.slice(base.length)}` : path;
}

/** The line of a folder found on this Mac; a folder with a server and a pipeline is named by
 * its server, as Use it uses the server. */
export function foundLine(folder: ExistingFolder, home: string | null): string {
  const where = shortPath(folder.path, home);
  if (folder.server !== null) {
    const names = folder.server.accounts.map((a) => a.slug);
    if (names.length === 0) return `Notedown found a server folder in ${where}.`;
    if (names.length === 1) return `Notedown found a server folder in ${where} with the account ${names[0]}.`;
    const list = `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
    return `Notedown found a server folder in ${where} with the accounts ${list}.`;
  }
  const url = folder.pipeline?.url ?? null;
  return url === null
    ? `Notedown found an OCR connection in ${where}.`
    : `Notedown found an OCR connection in ${where} to ${url}.`;
}

/** The data folder for Another machine when a server folder is at ~/notedown-data: the OCR
 * keeps its work in ~/notedown-mac, apart from that server. null keeps the config's folder. */
export function remoteFolder(details: ExistingFolder[], home: string | null): string | null {
  const server = details.some((d) => d.server !== null && shortPath(d.path, home) === "~/notedown-data");
  return server ? "~/notedown-mac" : null;
}

/** The command that makes the account on the other machine. */
export function accountCommand(email: string): string {
  const typed = email.trim();
  const slug = slugFromEmail(typed);
  return `notedown cloud account --data ~/notedown-data add ${slug} --email ${typed === "" ? "you@example.org" : typed}`;
}

export interface NewAccount {
  email: string;
  slug: string;
  password: string;
  repeat: string;
}

/** What keeps Create account from running; null when the form is fine. */
export function createAccountProblem(form: NewAccount): string | null {
  if (form.email.trim() === "") return "Enter the e-mail.";
  if (form.slug.trim() === "") return "Enter the account name.";
  if (form.password === "") return "Enter a password.";
  if (/[\r\n]/.test(form.password)) return "The password must be one line.";
  if (form.password !== form.repeat) return "The two passwords are not the same.";
  return null;
}

/** The line step 1 shows after the first Local save turned Start at login on. */
export function autostartLine(before: AppConfig | null, after: AppConfig): string | null {
  if (before === null || before.setup.autostartOffered || !after.setup.autostartOffered) return null;
  return "Start at login is on, so the Supernote can always sync.";
}

/** The data folder step 1 starts with for `where`: the config's own folder in its own mode;
 * for Another machine away from a server folder at ~/notedown-data. */
export function startFolder(
  where: "local" | "remote",
  config: AppConfig,
  details: ExistingFolder[],
  home: string | null,
): string {
  if (where === config.mode || where === "local") return config.dataDir;
  return remoteFolder(details, home) ?? config.dataDir;
}

/** The found folder is the place the config already uses: Use it would change nothing. */
export function inUse(folder: ExistingFolder, config: AppConfig): boolean {
  const used = useFolder(folder);
  if (used.mode !== config.mode || used.dataDir !== config.dataDir) return false;
  return used.mode === "local" || (used.serverUrl ?? "") === (config.serverUrl ?? "");
}

/** Step 1 offers Start: a server on this Mac that does not run and that no import holds. Not
 * while its state is not known yet, and not while it stops. */
export function startOffered(input: ChecklistInput): boolean {
  const state = input.status.server.state?.state;
  return (
    input.config.mode === "local" &&
    !holdsServer(input.job) &&
    state !== undefined &&
    state !== "running" &&
    state !== "starting" &&
    state !== "stopping"
  );
}

export type AccountForm = "logIn" | "create";

/** What the button of step 2 does: Create an account only on this Mac's server; a field
 * that is missing is said, never ignored. */
export function accountAction(fields: {
  mode: AppConfig["mode"];
  form: AccountForm;
  email: string;
  slug: string;
  password: string;
  repeat: string;
}): { kind: AccountForm; problem: string | null } {
  if (fields.form === "create" && fields.mode === "local") {
    return { kind: "create", problem: createAccountProblem(fields) };
  }
  if (fields.mode === "notSet") return { kind: "logIn", problem: NO_MODE };
  const missing = fields.email.trim() === "" || fields.password === "";
  return { kind: "logIn", problem: missing ? "Enter the e-mail and the password." : null };
}

export interface CheckRow {
  title: string;
  value: string;
  tone: "secondary" | "good" | "bad";
}

const YES: Pick<CheckRow, "value" | "tone"> = { value: "Yes", tone: "good" };
const UNKNOWN: Pick<CheckRow, "value" | "tone"> = { value: "Not known yet", tone: "secondary" };

/** The answers of the connection checks of step 3. The app keeps only write tokens, so a
 * login the server takes has write access. */
export function connectionRows(status: Status, config: AppConfig, hasLogin: boolean): CheckRow[] {
  const c = status.connection;
  const answers =
    c === "reached" || c === "refused"
      ? YES
      : c === "unreachable" || c === "failed"
        ? { value: "No", tone: "bad" as const }
        : UNKNOWN;
  const login = c === "reached" ? YES : c === "refused" ? { value: "No. Log in again.", tone: "bad" as const } : UNKNOWN;
  const rows: CheckRow[] = [
    { title: "The server answers", ...answers },
    { title: "The login works", ...login },
    { title: "Write access", ...(c === "reached" && hasLogin ? YES : UNKNOWN) },
  ];
  if (config.mode === "remote") {
    rows.push({
      title: "The OCR on this Mac reads from the server",
      ...(status.existing.pipeline ? YES : { value: "Not connected yet", tone: "bad" as const }),
    });
  }
  return rows;
}
