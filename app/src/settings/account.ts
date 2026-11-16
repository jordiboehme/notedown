import type { Mode } from "../lib/api";

// What the Account page offers: the view model of its Log in and Log out buttons.

export interface AccountState {
  /** The mode of the config; null while the config is not read yet. */
  mode: Mode | null;
  /** The account of the config. It stays after Log out. */
  account: string | null;
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
  let note: string;
  if (state.busy === "logIn") note = "Logging in.";
  else if (!ready) note = NO_MODE;
  else if (state.account) note = `Logging in as another account replaces ${state.account}.`;
  else note = "Notedown keeps a write token in the Keychain of this Mac.";
  return {
    logIn: { enabled: state.busy === null && ready && filled, note },
    logOut: { enabled: state.busy === null && state.account !== null },
  };
}
