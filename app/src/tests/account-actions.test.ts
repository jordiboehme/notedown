import { beforeEach, describe, expect, it, vi } from "vitest";
import { accountActions, logInFlow, useFolder, type AccountState } from "../settings/account";
import type { AppConfig, ConfigAnswer, ExistingFolder } from "../settings/api";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (command: string, args?: unknown) => invoke(command, args) }));

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
const LEAVES = "Deletes the login from this Mac and forgets the account. The notes stay where they are.";

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

  it("shows write access only with a login", () => {
    const loggedIn = accountActions({ ...base, account: "jordi", accountEmail: "jordi@example.org", loggedIn: true });
    expect(loggedIn.writeAccess).toBe("Yes");
    // No row at all without a login.
    expect(accountActions(base).writeAccess).toBeNull();
    expect(accountActions({ ...base, account: "jordi", accountEmail: "jordi@example.org" }).writeAccess).toBeNull();
  });

  it("says that log out leaves the OCR connection on another machine", () => {
    const OCR = "Log out leaves the OCR connection to the other machine.";
    const remote = accountActions({
      ...base,
      mode: "remote",
      account: "jordi",
      accountEmail: "jordi@example.org",
      loggedIn: true,
    });
    expect(remote.logOut.note).toBe(`${LEAVES} ${OCR}`);
    const local = accountActions({ ...base, account: "jordi", accountEmail: "jordi@example.org", loggedIn: true });
    expect(local.logOut.note).toBe(LEAVES);
    expect(accountActions({ ...base, mode: "remote" }).logOut.note).toBe(LEAVES);
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

const server: ExistingFolder = {
  path: "/Users/me/notedown-data",
  server: { accounts: [{ slug: "me", email: "me@example.org", imported: true }] },
  pipeline: null,
};

describe("useFolder", () => {
  it("uses a server folder on this Mac with its one account", () => {
    expect(useFolder(server)).toEqual({
      mode: "local",
      dataDir: "/Users/me/notedown-data",
      serverUrl: null,
      email: "me@example.org",
    });
  });

  it("names no e-mail for a server folder with two accounts", () => {
    const two: ExistingFolder = {
      ...server,
      server: {
        accounts: [
          { slug: "anna", email: "anna@example.org", imported: false },
          { slug: "me", email: "me@example.org", imported: true },
        ],
      },
    };
    const used = useFolder(two);
    expect(used).toEqual({ mode: "local", dataDir: "/Users/me/notedown-data", serverUrl: null });
    expect(used.email).toBeUndefined();
  });

  it("uses a pipeline folder with the address of its server", () => {
    const pipeline: ExistingFolder = {
      path: "/Users/me/notedown-mac",
      server: null,
      pipeline: { url: "http://nas:19072" },
    };
    expect(useFolder(pipeline)).toEqual({
      mode: "remote",
      dataDir: "/Users/me/notedown-mac",
      serverUrl: "http://nas:19072",
    });
  });
});

const config: AppConfig = {
  version: 1,
  mode: "remote",
  dataDir: "/Users/me/notedown-mac",
  serverUrl: "http://nas:19072",
  account: null,
  email: null,
  startView: "tree",
  markdownDirs: {},
  setup: {
    backupUrl: null,
    backupDir: null,
    snbak: null,
    setupShown: false,
    autostartOffered: false,
    syncMark: null,
    syncChecked: null,
    syncAcknowledged: false,
  },
};

const answer = (account: string): ConfigAnswer => ({
  config: { ...config, account, email: "me@example.org" },
  keychain: null,
});

describe("logInFlow", () => {
  beforeEach(() => {
    invoke.mockReset();
  });

  it("connects the pipeline with the same password on another machine without one", async () => {
    invoke.mockImplementation(async (command: string) => answer(command === "log_in" ? "me" : "me-connected"));
    const outcome = await logInFlow("me@example.org", "pw", config, { server: false, pipeline: false });
    expect(invoke.mock.calls).toEqual([
      ["log_in", { email: "me@example.org", password: "pw" }],
      ["connect_pipeline", { password: "pw" }],
    ]);
    expect(outcome).toEqual({ loggedIn: true, answer: answer("me-connected"), connectError: null });
  });

  it("logs in only on this Mac or with a pipeline already there", async () => {
    invoke.mockImplementation(async () => answer("me"));
    await logInFlow("me@example.org", "pw", { ...config, mode: "local" }, { server: true, pipeline: false });
    await logInFlow("me@example.org", "pw", config, { server: false, pipeline: true });
    expect(invoke.mock.calls.map((c) => c[0])).toEqual(["log_in", "log_in"]);
  });

  it("keeps the login when the connection fails", async () => {
    invoke.mockImplementation(async (command: string) => {
      if (command === "connect_pipeline") throw "the server refused the email or the password";
      return answer("me");
    });
    const outcome = await logInFlow("me@example.org", "pw", config, { server: false, pipeline: false });
    expect(outcome).toEqual({
      loggedIn: true,
      answer: answer("me"),
      connectError: "the server refused the email or the password",
    });
  });

  it("fails as the login does and connects nothing", async () => {
    invoke.mockImplementation(async () => {
      throw "The server refused the e-mail or the password.";
    });
    await expect(logInFlow("me@example.org", "pw", config, { server: false, pipeline: false })).rejects.toBe(
      "The server refused the e-mail or the password.",
    );
    expect(invoke).toHaveBeenCalledTimes(1);
  });
});
