import { describe, expect, it } from "vitest";
import { accountActions, type AccountState } from "../settings/account";

const base: AccountState = {
  mode: "local",
  account: null,
  email: "me@example.org",
  password: "secret",
  busy: null,
};

describe("accountActions", () => {
  it("lets a filled form log in and keeps Log out off without an account", () => {
    const actions = accountActions(base);
    expect(actions.logIn.enabled).toBe(true);
    expect(actions.logIn.note).toBe("Notedown keeps a write token in the Keychain of this Mac.");
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
    const actions = accountActions({ ...base, mode: "remote", account: "jordi" });
    expect(actions.logIn.enabled).toBe(true);
    expect(actions.logIn.note).toBe("Logging in as another account replaces jordi.");
    expect(actions.logOut.enabled).toBe(true);
  });

  it("does nothing else while a login or a log out runs", () => {
    const actions = accountActions({ ...base, account: "jordi", busy: "logIn" });
    expect(actions.logIn.enabled).toBe(false);
    expect(actions.logIn.note).toBe("Logging in.");
    expect(actions.logOut.enabled).toBe(false);
    const leaving = accountActions({ ...base, account: "jordi", busy: "logOut" });
    expect(leaving.logIn.enabled).toBe(false);
    expect(leaving.logIn.note).toBe("Logging in as another account replaces jordi.");
    expect(leaving.logOut.enabled).toBe(false);
  });
});
