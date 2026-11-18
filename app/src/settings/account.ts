import type { Mode, Status } from "../lib/api";
import { type AppConfig, type ConfigAnswer, connectPipeline, type ExistingFolder, failure, logIn } from "./api";

// What the Account page offers: the view model of its Log in and Log out buttons.

export interface AccountState {
  /** The mode of the config; null while the config is not read yet. */
  mode: Mode | null;
  /** The account and the e-mail of the config. */
  account: string | null;
  accountEmail: string | null;
  /** The Keychain keeps a token for the account. */
  loggedIn: boolean;
  /** The fields of the form. */
  email: string;
  password: string;
  /** What runs now, if anything. */
  busy: "logIn" | "logOut" | null;
}

export interface AccountActions {
  logIn: { enabled: boolean; note: string };
  logOut: { enabled: boolean; note: string };
  /** The value of the Write access row; null hides the row (no login). */
  writeAccess: string | null;
}

export const NO_MODE = "Choose where the server runs first.";
const LEAVES = "Deletes the login from this Mac and forgets the account. The notes stay where they are.";
const OCR_STAYS = "Log out leaves the OCR connection to the other machine.";

export function accountActions(state: AccountState): AccountActions {
  const ready = state.mode !== null && state.mode !== "notSet";
  const filled = state.email.trim() !== "" && state.password !== "";
  const typed = state.email.trim().toLowerCase();
  const another =
    state.loggedIn && state.account !== null && typed !== (state.accountEmail ?? "").trim().toLowerCase();
  let note: string;
  if (state.busy === "logIn") note = "Logging in.";
  else if (!ready) note = NO_MODE;
  else if (another) note = `Logging in as another account replaces ${state.account}.`;
  else note = "Notedown keeps a write token in the Keychain of this Mac.";
  return {
    logIn: { enabled: state.busy === null && ready && filled, note },
    // An account in the config without a token can be logged out too: it forgets the account.
    // The pipeline's own login on another machine (`connect`) is not the app's to end.
    logOut: {
      enabled: state.busy === null && (state.loggedIn || state.account !== null),
      note: state.loggedIn && state.mode === "remote" ? `${LEAVES} ${OCR_STAYS}` : LEAVES,
    },
    // The app keeps only write tokens: a login refused write access is not kept.
    writeAccess: state.loggedIn ? "Yes" : null,
  };
}

/** What a login with the setup's password form did. The login worked; connectError is the
 * error of the pipeline connection that followed it, if any, for step 3 to show. */
export interface LogInOutcome {
  loggedIn: true;
  answer: ConfigAnswer;
  connectError: string | null;
}

/** Logs in; for a server on another machine without a pipeline connection on this Mac, or
 * whose server refused the pipeline's token (`tokenRefused`, after a password change), then
 * also connects the pipeline with the same password. A failed login throws as logIn does. */
export async function logInFlow(
  email: string,
  password: string,
  cfg: AppConfig,
  existing: Status["existing"],
  tokenRefused: boolean,
): Promise<LogInOutcome> {
  const answer = await logIn(email, password);
  if (cfg.mode !== "remote" || (existing.pipeline && !tokenRefused)) {
    return { loggedIn: true, answer, connectError: null };
  }
  try {
    return { loggedIn: true, answer: await connectPipeline(password), connectError: null };
  } catch (error) {
    return { loggedIn: true, answer, connectError: failure(error) };
  }
}

/** The place "Use it" sets for a folder found on this Mac. */
export interface UsedFolder {
  mode: "local" | "remote";
  dataDir: string;
  serverUrl: string | null;
  /** The e-mail for the login form: only when the server folder holds exactly one account. */
  email?: string;
}

/** "Use it": a server folder runs the server on this Mac with that folder; a pipeline folder
 * reads from the server it names. */
export function useFolder(folder: ExistingFolder): UsedFolder {
  if (folder.server !== null) {
    const accounts = folder.server.accounts;
    const used: UsedFolder = { mode: "local", dataDir: folder.path, serverUrl: null };
    if (accounts.length === 1) used.email = accounts[0].email;
    return used;
  }
  return { mode: "remote", dataDir: folder.path, serverUrl: folder.pipeline?.url ?? null };
}
