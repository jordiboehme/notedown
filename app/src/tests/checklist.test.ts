import { describe, expect, it } from "vitest";
import type { Status } from "../lib/api";
import type { AppConfig, ExistingFolder, JobKind, ModelState } from "../settings/api";
import {
  OUTSIDE_LINE,
  accountCommand,
  autostartLine,
  accountAction,
  checklist,
  connectionRows,
  createAccountProblem,
  foundLine,
  inUse,
  ocrConnect,
  RECONNECT_NOTE,
  remoteFolder,
  shortPath,
  startFolder,
  startOffered,
  stepDot,
  tokenKey,
  type ChecklistInput,
  type Step,
  type StepId,
} from "../settings/checklist";
import { notSetUp, readyModel, sampleConfig, sampleStatus } from "./sample";

const NOW = 1_800_000_000_000;
const HOME = "/Users/me";

function input(change: Partial<ChecklistInput> = {}): ChecklistInput {
  const status = change.status ?? sampleStatus(NOW);
  return {
    status,
    config: sampleConfig(),
    model: readyModel(),
    existing: status.existing,
    details: [],
    job: null,
    hasLogin: true,
    ...change,
  };
}

function byId(steps: Step[]): Record<StepId, Step> {
  return Object.fromEntries(steps.map((s) => [s.id, s])) as Record<StepId, Step>;
}

function states(steps: Step[]): Record<string, string> {
  return Object.fromEntries(steps.map((s) => [s.id, s.state]));
}

/** A Local setup logged in with the model, before the import and the first sync. */
function beforeImport(): { status: Status; config: AppConfig } {
  const status = sampleStatus(NOW);
  status.account = { imported: false, appDataImported: false };
  // The Supernote cannot sync before the import (the server protects the account).
  status.device.lastSyncMs = null;
  return { status, config: sampleConfig() };
}

const CONFIG_NOT_SET = (): AppConfig => ({ ...sampleConfig(), mode: "notSet", account: null, email: null });

describe("checklist", () => {
  it("has the seven steps in order", () => {
    expect(checklist(input()).map((s) => s.id)).toEqual([
      "server",
      "account",
      "connection",
      "model",
      "import",
      "supernote",
      "agent",
    ]);
  });

  it("a new mac has every step blocked behind the server", () => {
    const steps = byId(
      checklist(input({ status: notSetUp(), config: CONFIG_NOT_SET(), model: null, hasLogin: false })),
    );
    expect(steps.server.state).toBe("todo");
    expect(steps.server.line).toBe("Choose where the server runs.");
    for (const id of ["account", "connection", "import", "supernote"] as const) {
      expect(steps[id].state, id).toBe("blocked");
      expect(steps[id].blockedBy, id).toBe("server");
      expect(steps[id].line, id).toBe("Do step 1 first.");
    }
    // The model needs no server: it can be downloaded at once.
    expect(steps.model.state).toBe("todo");
    expect(steps.model.blockedBy).toBeUndefined();
    expect(steps.agent.state).toBe("optional");
    expect(steps.agent.blockedBy).toBeUndefined();
  });

  it("a local server with a login and the model leaves import and supernote", () => {
    const steps = byId(checklist(input(beforeImport())));
    expect(states(Object.values(steps))).toEqual({
      server: "done",
      account: "done",
      connection: "done",
      model: "done",
      import: "todo",
      supernote: "blocked",
      agent: "done",
    });
    expect(steps.supernote.blockedBy).toBe("import");
    expect(steps.supernote.line).toBe("Do step 5 first.");
  });

  it("a supernote that synced before the import does not make step 6 done", () => {
    const { status, config } = beforeImport();
    status.device.lastSyncMs = NOW - 60_000;
    config.setup.syncChecked = { files: 0, folders: 0, digests: 0, cannotTell: false };
    const steps = byId(checklist(input({ status, config })));
    expect(steps.import.state).toBe("todo");
    expect(steps.supernote.state).toBe("blocked");
    expect(steps.supernote.blockedBy).toBe("import");
  });

  it("an older server shows update the server on step 5", () => {
    const { status, config } = beforeImport();
    config.mode = "remote";
    config.serverUrl = "http://pi.local:19072";
    status.setup.mode = "remote";
    status.server.state = null;
    status.account = { imported: null, appDataImported: null };
    const steps = byId(checklist(input({ status, config })));
    expect(steps.import.state).toBe("todo");
    expect(steps.import.line).toBe("Update the server on the other machine.");
    // Step 6 does not open before step 5, also when the Supernote synced.
    expect(steps.supernote.state).toBe("blocked");
    expect(steps.supernote.blockedBy).toBe("import");
    status.device.lastSyncMs = NOW;
    expect(byId(checklist(input({ status, config }))).supernote.state).toBe("blocked");
  });

  it("the agent step is optional and never blocks", () => {
    const status = notSetUp();
    let steps = byId(checklist(input({ status, config: CONFIG_NOT_SET(), hasLogin: false })));
    expect(steps.agent.state).toBe("optional");
    // An agent that reads makes it done, whatever the other steps say.
    status.agents = sampleStatus(NOW).agents;
    steps = byId(checklist(input({ status, config: CONFIG_NOT_SET(), hasLogin: false })));
    expect(steps.agent.state).toBe("done");
    // Without an agent, every other step can be done: nothing waits for step 7.
    const all = sampleStatus(NOW);
    all.agents = [];
    const config = sampleConfig();
    config.setup.syncMark = { key: "local|me", seq: 40, atMs: 1, syncAtMark: null };
    config.setup.syncChecked = { files: 0, folders: 0, digests: 0, cannotTell: false };
    steps = byId(checklist(input({ status: all, config })));
    expect(steps.agent.state).toBe("optional");
    for (const id of ["server", "account", "connection", "model", "import", "supernote"] as const) {
      expect(steps[id].state, id).toBe("done");
    }
  });

  it("a running import shows working, and the stopped server does not block it", () => {
    for (const job of ["import", "appData"] as JobKind[]) {
      const { status, config } = beforeImport();
      // The import stopped the app's server: it does not answer while the import runs.
      status.server.state = { state: "stopped" };
      status.connection = "unreachable";
      status.account = null;
      const steps = byId(checklist(input({ status, config, job })));
      expect(steps.server.state, job).toBe("done");
      expect(steps.connection.state, job).toBe("done");
      expect(steps.connection.line, job).toBe("The import holds the server.");
      expect(steps.import.state, job).toBe("working");
      expect(steps.import.blockedBy, job).toBeUndefined();
      expect(steps.supernote.state, job).toBe("blocked");
    }
    // A backup does not hold the server: the stopped server is still to do.
    const { status, config } = beforeImport();
    status.server.state = { state: "stopped" };
    status.server.line = "Stopped";
    const steps = byId(checklist(input({ status, config, job: "backup" })));
    expect(steps.server.state).toBe("todo");
    expect(steps.server.line).toBe("The server is stopped. Press Start.");
    expect(steps.import.state).toBe("working");
  });

  it("an import that was cut shows not done and asks for Replace", () => {
    const { status, config } = beforeImport();
    const steps = byId(checklist(input({ status, config, dryRun: { filesPresent: 120 } })));
    expect(steps.import.state).toBe("todo");
    expect(steps.import.line).toBe("The last import did not finish. Import again with Replace.");
    // A dry run on an empty server is the first import, not a cut one.
    const fresh = byId(checklist(input({ status, config, dryRun: { filesPresent: 0 } })));
    expect(fresh.import.line).not.toContain("Replace");
  });

  it("a remote server that refuses the login still counts as reached for step 1", () => {
    const status = sampleStatus(NOW);
    const config = sampleConfig();
    config.mode = "remote";
    config.serverUrl = "http://pi.local:19072";
    status.setup.mode = "remote";
    status.server.state = null;
    status.connection = "refused";
    status.account = null;
    const steps = byId(checklist(input({ status, config })));
    expect(steps.server.state).toBe("done");
    expect(steps.connection.state).toBe("todo");
    expect(steps.connection.line).toBe("The server refused the login. Log in again.");
    status.connection = "unreachable";
    const away = byId(checklist(input({ status, config })));
    expect(away.server.state).toBe("todo");
    expect(away.server.line).toBe("The server does not answer.");
  });

  it("an outside server shows the outside line on step 1", () => {
    const status = sampleStatus(NOW);
    status.server.state = { state: "outside", what: "server" };
    const steps = byId(checklist(input({ status })));
    expect(steps.server.state).toBe("todo");
    expect(steps.server.line).toBe(OUTSIDE_LINE);
    expect(steps.account.state).toBe("blocked");
  });

  it("the connection step names what is missing", () => {
    const remote = () => {
      const status = sampleStatus(NOW);
      const config = sampleConfig();
      config.mode = "remote";
      config.serverUrl = "http://pi.local:19072";
      status.setup.mode = "remote";
      status.server.state = null;
      return { status, config };
    };
    const { status, config } = remote();
    status.existing = { server: false, pipeline: false };
    let steps = byId(checklist(input({ status, config, existing: status.existing })));
    expect(steps.connection.state).toBe("todo");
    expect(steps.connection.line).toBe("The OCR is not connected to the server yet.");
    status.existing = { server: false, pipeline: true };
    steps = byId(checklist(input({ status, config, existing: status.existing })));
    expect(steps.connection.state).toBe("done");
    // The server refused the OCR's token (a password change): step 3 is to do again.
    status.ocr.tokenRefused = true;
    steps = byId(checklist(input({ status, config, existing: status.existing })));
    expect(steps.connection.state).toBe("todo");
    expect(steps.connection.line).toBe("The server no longer accepts the OCR's login.");
    status.ocr.tokenRefused = false;
    // This Mac's server does not answer.
    const local = sampleStatus(NOW);
    local.connection = "unreachable";
    local.account = null;
    steps = byId(checklist(input({ status: local })));
    expect(steps.connection.line).toBe("The server does not answer.");
  });

  it("the account step waits for a login", () => {
    const steps = byId(checklist(input({ hasLogin: false })));
    expect(steps.account.state).toBe("todo");
    expect(steps.connection.state).toBe("blocked");
    expect(steps.connection.blockedBy).toBe("account");
    expect(steps.connection.line).toBe("Do step 2 first.");
  });

  it("the model step shows a download as working", () => {
    const model: ModelState = { ...readyModel(), installed: false, pulling: true };
    expect(byId(checklist(input({ model }))).model.state).toBe("working");
    const missing: ModelState = { ...readyModel(), installed: false, sizeBytes: 0 };
    expect(byId(checklist(input({ model: missing }))).model.state).toBe("todo");
  });

  it("the import and the first sync do not wait for the model", () => {
    const { status, config } = beforeImport();
    const missing: ModelState = { ...readyModel(), installed: false, sizeBytes: 0 };
    let steps = byId(checklist(input({ status, config, model: missing })));
    expect(steps.model.state).toBe("todo");
    expect(steps.import.state).toBe("todo");
    steps = byId(checklist(input({ model: missing })));
    expect(steps.import.state).toBe("done");
    expect(steps.supernote.state).toBe("done");
  });

  it("a server that is not running asks for Start", () => {
    const status = sampleStatus(NOW);
    status.server.state = { state: "backoff", untilMs: NOW, lastError: "x" };
    expect(byId(checklist(input({ status }))).server.line).toBe("The server stopped with an error. Press Start.");
    expect(startOffered(input({ status }))).toBe(true);
    status.server.state = { state: "outside", what: "server" };
    expect(startOffered(input({ status }))).toBe(true);
    status.server.state = { state: "stopped" };
    expect(startOffered(input({ status }))).toBe(true);
    // Not while it runs or starts, not while an import holds it, not for another machine.
    expect(startOffered(input({ status, job: "import" }))).toBe(false);
    expect(startOffered(input({ status: sampleStatus(NOW) }))).toBe(false);
    status.server.state = { state: "starting" };
    expect(startOffered(input({ status }))).toBe(false);
    const remote = { ...sampleConfig(), mode: "remote" as const, serverUrl: "http://pi.local:19072" };
    expect(startOffered(input({ status: notSetUp(), config: remote }))).toBe(false);
    // Not while it stops, and not while its state is not known yet.
    status.server.state = { state: "stopping" };
    expect(startOffered(input({ status }))).toBe(false);
    status.server.state = null;
    expect(startOffered(input({ status }))).toBe(false);
    expect(byId(checklist(input({ status }))).server.line).toBe("Checking the server.");
  });

  it("the remote server line has no login in the address", () => {
    const config = { ...sampleConfig(), mode: "remote" as const, serverUrl: "https://u:secret@pi.local:19072" };
    const status = sampleStatus(NOW);
    status.setup.mode = "remote";
    status.server.state = null;
    const line = byId(checklist(input({ status, config }))).server.line;
    expect(line).toBe("The server at https://pi.local:19072 answers.");
  });

  it("never shows a step of the chain done after one that is not", () => {
    const chain: StepId[] = ["server", "account", "connection", "import", "supernote"];
    const fixtures: ChecklistInput[] = [];
    for (const status of [sampleStatus(NOW), notSetUp(), beforeImport().status]) {
      for (const config of [sampleConfig(), CONFIG_NOT_SET(), beforeImport().config]) {
        for (const hasLogin of [true, false]) {
          for (const job of [null, "import", "backup"] as (JobKind | null)[]) {
            for (const connection of ["reached", "refused", "unreachable"] as const) {
              const changed: Status = { ...status, connection };
              const synced = { ...config, setup: { ...config.setup, syncMark: { key: "local|me", seq: 1, atMs: 1, syncAtMark: null } } };
              for (const cfg of [config, synced]) {
                fixtures.push(input({ status: changed, config: cfg, hasLogin, job, model: null }));
              }
            }
          }
        }
      }
    }
    for (const fixture of fixtures) {
      const steps = byId(checklist(fixture));
      let open = true;
      for (const id of chain) {
        if (!open) expect(steps[id].state, JSON.stringify({ id, job: fixture.job })).not.toBe("done");
        if (steps[id].state !== "done") open = false;
      }
    }
  });

  it("the local import needs the files and the digests", () => {
    const { status, config } = beforeImport();
    status.account = { imported: true, appDataImported: false };
    const steps = byId(checklist(input({ status, config })));
    expect(steps.import.state).toBe("todo");
    expect(steps.import.line).toBe("The files are imported. Import the digests next.");
    // Another machine imports the digests there: the files are enough here.
    config.mode = "remote";
    config.serverUrl = "http://pi.local:19072";
    status.setup.mode = "remote";
    status.server.state = null;
    expect(byId(checklist(input({ status, config }))).import.state).toBe("done");
  });

  it("the first sync is done once its check is clean or seen", () => {
    const status = sampleStatus(NOW);
    const config = sampleConfig();
    config.setup.syncMark = { key: "local|me", seq: 40, atMs: 1, syncAtMark: null };
    // The mark is there, the check not yet.
    let steps = byId(checklist(input({ status, config })));
    expect(steps.supernote.state).toBe("todo");
    config.setup.syncChecked = { files: 3, folders: 0, digests: 0, cannotTell: false };
    steps = byId(checklist(input({ status, config })));
    expect(steps.supernote.state).toBe("todo");
    config.setup.syncAcknowledged = true;
    expect(byId(checklist(input({ status, config }))).supernote.state).toBe("done");
    // A check that cannot tell needs the same look.
    config.setup.syncChecked = { files: 0, folders: 0, digests: null, cannotTell: true };
    config.setup.syncAcknowledged = false;
    expect(byId(checklist(input({ status, config }))).supernote.state).toBe("todo");
  });

  it("a failed check of the first sync says why on step 6", () => {
    const status = sampleStatus(NOW);
    const config = sampleConfig();
    config.setup.syncMark = { key: "local|me", seq: 40, atMs: 1, syncAtMark: null };
    status.firstSyncError = "journal tail took too long.";
    const step = byId(checklist(input({ status, config }))).supernote;
    expect(step.state).toBe("todo");
    expect(step.line).toBe(
      "Notedown could not check the first sync: journal tail took too long. It tries again every minute.",
    );
    // Once checked, the check counts.
    config.setup.syncChecked = { files: 0, folders: 0, digests: 0, cannotTell: false };
    expect(byId(checklist(input({ status, config }))).supernote.line).toBe(
      "The first sync worked. Nothing was deleted.",
    );
  });

  it("import again keeps step 5 open and step 6 waiting until both imports are done", () => {
    const status = sampleStatus(NOW);
    const config = sampleConfig();
    config.setup.syncMark = { key: "local|me", seq: 40, atMs: 1, syncAtMark: null };
    config.setup.syncChecked = { files: 0, folders: 0, digests: 0, cannotTell: false };
    // The server still says imported from the first time.
    let steps = byId(checklist(input({ status, config, againRuns: true })));
    expect(steps.import.state).toBe("todo");
    expect(steps.supernote.state).toBe("blocked");
    expect(steps.supernote.blockedBy).toBe("import");
    // While a step of it runs, step 5 works and step 6 still waits.
    steps = byId(checklist(input({ status, config, againRuns: true, job: "importDryRun" })));
    expect(steps.import.state).toBe("working");
    expect(steps.supernote.state).toBe("blocked");
    // Both imports done: as before.
    steps = byId(checklist(input({ status, config, againRuns: false })));
    expect(steps.import.state).toBe("done");
    expect(steps.supernote.state).toBe("done");
  });

  it("an adopted setup without a mark is done after the first device sync", () => {
    const status = sampleStatus(NOW);
    const config = sampleConfig();
    const steps = byId(checklist(input({ status, config })));
    expect(steps.supernote.state).toBe("done");
    expect(steps.supernote.line).toBe(
      "Notedown did not check this first sync for deletes, because the import was done outside the app.",
    );
    status.device.lastSyncMs = null;
    expect(byId(checklist(input({ status, config }))).supernote.state).toBe("todo");
    // A mark of another account does not count as this one's.
    status.device.lastSyncMs = NOW;
    config.setup.syncMark = { key: "local|other", seq: 4, atMs: 1, syncAtMark: null };
    config.setup.syncChecked = { files: 0, folders: 0, digests: 0, cannotTell: false };
    expect(byId(checklist(input({ status, config }))).supernote.state).toBe("done");
  });

  it("another machine waits for its mark: an earlier sync never makes step 6 done", () => {
    const status = sampleStatus(NOW);
    status.setup.mode = "remote";
    status.server.state = null;
    const config = { ...sampleConfig(), mode: "remote" as const, serverUrl: "http://pi.local:19072" };
    // The poll that sees the import takes the mark; until then a sync does not count.
    let steps = byId(checklist(input({ status, config })));
    expect(steps.import.state).toBe("done");
    expect(steps.supernote.state).toBe("todo");
    config.setup.syncMark = { key: "http://pi.local:19072|me", seq: 4, atMs: NOW, syncAtMark: null };
    expect(byId(checklist(input({ status, config }))).supernote.state).toBe("todo");
    config.setup.syncChecked = { files: 0, folders: 0, digests: null, cannotTell: false };
    steps = byId(checklist(input({ status, config })));
    expect(steps.supernote.state).toBe("done");
    expect(steps.supernote.line).toBe("The first sync worked. Nothing was deleted.");
  });
});

describe("tokenKey", () => {
  it("is the key Rust keeps the login and the sync mark by", () => {
    const config = sampleConfig();
    expect(tokenKey(config)).toBe("local|me");
    config.mode = "remote";
    config.serverUrl = "https://user:secret@notes.example.org/x";
    expect(tokenKey(config)).toBe("https://notes.example.org/x|me");
    config.serverUrl = null;
    expect(tokenKey(config)).toBeNull();
    expect(tokenKey({ ...sampleConfig(), account: null })).toBeNull();
    expect(tokenKey({ ...sampleConfig(), mode: "notSet" })).toBeNull();
  });
});

describe("stepDot", () => {
  it("gives each state a dot and a word", () => {
    expect(stepDot("done")).toEqual({ level: "good", text: "Done" });
    expect(stepDot("todo")).toEqual({ level: "warning", text: "To do" });
    expect(stepDot("blocked")).toEqual({ level: "unknown", text: "Waiting" });
    expect(stepDot("working")).toEqual({ level: "warning", text: "Working" });
    expect(stepDot("optional")).toEqual({ level: "unknown", text: "Optional" });
  });
});

describe("the setup helpers", () => {
  const server: ExistingFolder = {
    path: "/Users/me/notedown-data",
    server: { accounts: [{ slug: "me", email: "me@example.org", imported: true }] },
    pipeline: null,
  };
  const pipeline: ExistingFolder = {
    path: "/Users/me/notedown-mac",
    server: null,
    pipeline: { url: "http://pi.local:19072" },
  };

  it("names each folder found on this Mac", () => {
    expect(foundLine(server, HOME)).toBe("Notedown found a server folder in ~/notedown-data with the account me.");
    expect(foundLine(pipeline, HOME)).toBe("Notedown found an OCR connection in ~/notedown-mac to http://pi.local:19072.");
    const two: ExistingFolder = {
      ...server,
      server: {
        accounts: [
          { slug: "a", email: "a@x.org", imported: true },
          { slug: "b", email: "b@x.org", imported: false },
        ],
      },
    };
    expect(foundLine(two, HOME)).toBe("Notedown found a server folder in ~/notedown-data with the accounts a and b.");
    expect(foundLine({ ...server, server: { accounts: [] } }, HOME)).toBe(
      "Notedown found a server folder in ~/notedown-data.",
    );
    expect(foundLine({ ...pipeline, pipeline: { url: null } }, HOME)).toBe(
      "Notedown found an OCR connection in ~/notedown-mac.",
    );
  });

  it("shortens the home folder the app names", () => {
    expect(shortPath("/Users/me/notedown-data", HOME)).toBe("~/notedown-data");
    expect(shortPath("/Users/me", HOME)).toBe("~");
    expect(shortPath("/Users/me", "/Users/me/")).toBe("~");
    expect(shortPath("/Users/meier/x", HOME)).toBe("/Users/meier/x");
    expect(shortPath("/Volumes/x/notedown", HOME)).toBe("/Volumes/x/notedown");
    expect(shortPath("/Users/me/notedown-data", null)).toBe("/Users/me/notedown-data");
    expect(shortPath("/home/me/n", "/home/me")).toBe("~/n");
  });

  it("offers Use it only for a folder that is not in use", () => {
    const config = sampleConfig();
    expect(inUse(server, config)).toBe(true);
    expect(inUse(pipeline, config)).toBe(false);
    expect(inUse(server, { ...config, mode: "remote", serverUrl: "http://pi.local:19072" })).toBe(false);
    const remote = { ...config, mode: "remote" as const, dataDir: "/Users/me/notedown-mac", serverUrl: "http://pi.local:19072" };
    expect(inUse(pipeline, remote)).toBe(true);
    expect(inUse(pipeline, { ...remote, serverUrl: "http://other:19072" })).toBe(false);
  });

  it("keeps the OCR of another machine out of a server folder", () => {
    expect(remoteFolder([server], HOME)).toBe("~/notedown-mac");
    expect(remoteFolder([server], null)).toBeNull();
    expect(remoteFolder([pipeline], HOME)).toBeNull();
    expect(remoteFolder([{ ...server, path: "/Volumes/x/notes" }], HOME)).toBeNull();
    expect(remoteFolder([], HOME)).toBeNull();
  });

  it("gives the command that makes the account on the other machine", () => {
    expect(accountCommand("Jordi.Boehme@example.org")).toBe(
      "notedown cloud account --data ~/notedown-data add jordi-boehme --email Jordi.Boehme@example.org",
    );
    expect(accountCommand("  ")).toBe("notedown cloud account --data ~/notedown-data add me --email you@example.org");
  });

  it("checks the form of a new account", () => {
    const form = { email: "me@example.org", slug: "me", password: "secret", repeat: "secret" };
    expect(createAccountProblem(form)).toBeNull();
    expect(createAccountProblem({ ...form, email: " " })).toBe("Enter the e-mail.");
    expect(createAccountProblem({ ...form, slug: "" })).toBe("Enter the account name.");
    expect(createAccountProblem({ ...form, password: "", repeat: "" })).toBe("Enter a password.");
    expect(createAccountProblem({ ...form, repeat: "other" })).toBe("The two passwords are not the same.");
    expect(createAccountProblem({ ...form, password: "a\nb", repeat: "a\nb" })).toBe(
      "The password must be one line.",
    );
  });

  it("lets the button of step 2 always answer", () => {
    const fields = { mode: "local" as const, form: "create" as const, email: "me@x.org", slug: "me", password: "a", repeat: "b" };
    expect(accountAction(fields)).toEqual({ kind: "create", problem: "The two passwords are not the same." });
    // A Create form left from This Mac logs in on another machine.
    expect(accountAction({ ...fields, mode: "remote" })).toEqual({ kind: "logIn", problem: null });
    expect(accountAction({ ...fields, mode: "remote", password: "" })).toEqual({
      kind: "logIn",
      problem: "Enter the e-mail and the password.",
    });
    expect(accountAction({ ...fields, form: "logIn", email: "" }).problem).toBe("Enter the e-mail and the password.");
    expect(accountAction({ ...fields, mode: "notSet", form: "logIn" }).problem).toBe("Choose where the server runs first.");
  });

  it("says once that Start at login is on", () => {
    const before = { ...sampleConfig(), mode: "notSet" as const };
    before.setup = { ...before.setup, autostartOffered: false };
    const after = sampleConfig();
    expect(autostartLine(before, after)).toBe("Start at login is on, so the Supernote can always sync.");
    expect(autostartLine(after, after)).toBeNull();
    expect(autostartLine(null, after)).toBeNull();
  });
});

describe("startFolder", () => {
  const server: ExistingFolder = {
    path: "/Users/me/notedown-data",
    server: { accounts: [] },
    pipeline: null,
  };

  it("keeps the config's folder for its own mode and moves the OCR out of a server folder", () => {
    const config = sampleConfig();
    expect(startFolder("local", config, [server], HOME)).toBe("/Users/me/notedown-data");
    expect(startFolder("remote", config, [server], HOME)).toBe("~/notedown-mac");
    expect(startFolder("remote", config, [], HOME)).toBe("/Users/me/notedown-data");
    const remote = { ...config, mode: "remote" as const, dataDir: "/Users/me/ocr" };
    expect(startFolder("remote", remote, [server], HOME)).toBe("/Users/me/ocr");
  });
});

describe("connectionRows", () => {
  it("answers each check in plain words", () => {
    const status = sampleStatus(NOW);
    const config = sampleConfig();
    expect(connectionRows(status, config, true)).toEqual([
      { title: "The server answers", value: "Yes", tone: "good" },
      { title: "The login works", value: "Yes", tone: "good" },
      { title: "Write access", value: "Yes", tone: "good" },
    ]);
    status.connection = "refused";
    expect(connectionRows(status, config, true)).toEqual([
      { title: "The server answers", value: "Yes", tone: "good" },
      { title: "The login works", value: "No. Log in again.", tone: "bad" },
      { title: "Write access", value: "Not known yet", tone: "secondary" },
    ]);
    status.connection = "unreachable";
    expect(connectionRows(status, config, false).map((r) => r.value)).toEqual([
      "No",
      "Not known yet",
      "Not known yet",
    ]);
  });

  it("adds the OCR connection for a server on another machine", () => {
    const status = sampleStatus(NOW);
    const config = { ...sampleConfig(), mode: "remote" as const, serverUrl: "http://pi.local:19072" };
    status.existing = { server: false, pipeline: false };
    expect(connectionRows(status, config, true).at(-1)).toEqual({
      title: "The OCR on this Mac reads from the server",
      value: "Not connected yet",
      tone: "bad",
    });
    status.existing = { server: false, pipeline: true };
    expect(connectionRows(status, config, true).at(-1)?.value).toBe("Yes");
    status.ocr.tokenRefused = true;
    expect(connectionRows(status, config, true).at(-1)).toEqual({
      title: "The OCR on this Mac reads from the server",
      value: "No. Connect again.",
      tone: "bad",
    });
  });
});

describe("ocrConnect", () => {
  const remote = (): AppConfig => ({ ...sampleConfig(), mode: "remote", serverUrl: "http://pi.local:19072" });

  it("offers to connect the OCR on another machine without a connection", () => {
    const status = sampleStatus(NOW);
    status.existing = { server: false, pipeline: false };
    expect(ocrConnect(status, remote())).toEqual({
      again: false,
      note: "The OCR on this Mac logs in to the server with your password.",
      button: "Connect",
    });
  });

  it("offers to connect again when the server refused the OCR's token", () => {
    const status = sampleStatus(NOW);
    status.existing = { server: false, pipeline: true };
    expect(ocrConnect(status, remote())).toBeNull();
    status.ocr.tokenRefused = true;
    expect(ocrConnect(status, remote())).toEqual({ again: true, note: RECONNECT_NOTE, button: "Connect again" });
    expect(RECONNECT_NOTE).toBe(
      "The server no longer accepts the OCR's login. This happens after a password change. Enter the password to connect again.",
    );
  });

  it("offers nothing on this Mac's server or without a status", () => {
    const status = sampleStatus(NOW);
    status.ocr.tokenRefused = true;
    status.existing = { server: true, pipeline: false };
    expect(ocrConnect(status, sampleConfig())).toBeNull();
    expect(ocrConnect(null, remote())).toBeNull();
    expect(ocrConnect(status, null)).toBeNull();
  });
});

describe("the OCR page", () => {
  it("shows Connect again with the password field when the server refused the OCR's token", async () => {
    const { render } = await import("svelte/server");
    const { status } = await import("../lib/status.svelte");
    const { config } = await import("../settings/config.svelte");
    const { default: OcrPage } = await import("../settings/pages/OcrPage.svelte");
    const current = sampleStatus(NOW);
    current.existing = { server: false, pipeline: true };
    config.current = { ...sampleConfig(), mode: "remote", serverUrl: "http://pi.local:19072" };
    status.current = current;
    expect(render(OcrPage, { props: { now: NOW } }).body).not.toContain("Connect again");
    status.current = { ...current, ocr: { ...current.ocr, tokenRefused: true } };
    const body = render(OcrPage, { props: { now: NOW } }).body;
    expect(body).toContain("Connect again");
    expect(body).toContain("This happens after a password change. Enter the password to connect again.");
    expect(body).toContain('type="password"');
    status.current = null;
    config.current = null;
  });
});

describe("the setup page", () => {
  it("shows each step with its number, its title and its line", async () => {
    const { render } = await import("svelte/server");
    const { status } = await import("../lib/status.svelte");
    const { config } = await import("../settings/config.svelte");
    const { default: SetupPage } = await import("../settings/pages/SetupPage.svelte");
    status.current = notSetUp();
    config.current = CONFIG_NOT_SET();
    const body = render(SetupPage, { props: { onOpen: () => {} } }).body;
    expect(body).toContain("1. Choose the server");
    expect(body).toContain("Choose where the server runs.");
    expect(body).toContain("Do step 1 first.");
    expect(body).toContain("7. Connect an agent");
    // A step that waits shows no controls: no login form before the server.
    expect(body).not.toContain("Create account");
    status.current = null;
    config.current = null;
  });
});
