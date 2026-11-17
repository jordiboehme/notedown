import { describe, expect, it } from "vitest";
import { accountActions, type AccountState } from "../settings/account";

const base: AccountState = {
  mode: "local",
  account: null,
  accountEmail: null,
  loggedIn: false,
  email: "me@example.org",
  password: "secret",
  busy: null,
};

const KEEPS = "Notedown keeps a write token in the Keychain of this Mac.";

describe("accountActions", () => {
  it("lets a filled form log in and keeps Log out off without a login", () => {
    const actions = accountActions(base);
    expect(actions.logIn.enabled).toBe(true);
    expect(actions.logIn.note).toBe(KEEPS);
    expect(actions.logOut.enabled).toBe(false);
  });

  it("refuses a login without a mode", () => {
    for (const mode of ["notSet", null] as const) {
      const actions = accountActions({ ...base, mode });
      expect(actions.logIn.enabled).toBe(false);
      expect(actions.logIn.note).toBe("Choose where the server runs first.");
    }
  });

  it("needs the e-mail and the password", () => {
    expect(accountActions({ ...base, email: "  " }).logIn.enabled).toBe(false);
    expect(accountActions({ ...base, password: "" }).logIn.enabled).toBe(false);
  });

  it("says that another account replaces the one logged in", () => {
    const actions = accountActions({
      ...base,
      mode: "remote",
      account: "jordi",
      accountEmail: "jordi@example.org",
      loggedIn: true,
    });
    expect(actions.logIn.enabled).toBe(true);
    expect(actions.logIn.note).toBe("Logging in as another account replaces jordi.");
    expect(actions.logOut.enabled).toBe(true);
  });

  it("does not speak of replacing for the same account or without a login", () => {
    const same = accountActions({
      ...base,
      account: "jordi",
      accountEmail: "Me@Example.org",
      loggedIn: true,
      email: " me@example.org ",
    });
    expect(same.logIn.note).toBe(KEEPS);
    expect(same.logOut.enabled).toBe(true);
    // An account in the config without a token: nothing to replace, but Log out still forgets
    // the account (the Keychain read may also have failed).
    const noToken = accountActions({ ...base, account: "jordi", accountEmail: "jordi@example.org" });
    expect(noToken.logIn.note).toBe(KEEPS);
    expect(noToken.logOut.enabled).toBe(true);
  });

  it("does nothing else while a login or a log out runs", () => {
    const loggedIn = { ...base, account: "jordi", accountEmail: "jordi@example.org", loggedIn: true };
    const actions = accountActions({ ...loggedIn, busy: "logIn" });
    expect(actions.logIn.enabled).toBe(false);
    expect(actions.logIn.note).toBe("Logging in.");
    expect(actions.logOut.enabled).toBe(false);
    const leaving = accountActions({ ...loggedIn, busy: "logOut" });
    expect(leaving.logIn.enabled).toBe(false);
    expect(leaving.logOut.enabled).toBe(false);
  });
});
