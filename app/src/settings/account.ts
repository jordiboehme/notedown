import type { Mode } from "../lib/api";

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
  logOut: { enabled: boolean };
}

export const NO_MODE = "Choose where the server runs first.";

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
    logOut: { enabled: state.busy === null && (state.loggedIn || state.account !== null) },
  };
}
