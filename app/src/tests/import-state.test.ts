import { beforeEach, describe, expect, it, vi } from "vitest";

const calls: { command: string; args: Record<string, unknown> | undefined }[] = [];

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn((command: string, args?: Record<string, unknown>) => {
    calls.push({ command, args });
    if (command === "set_setup_record") return Promise.resolve(null);
    if (command === "backup_folder") return Promise.resolve({ state: "missing" });
    return Promise.resolve(null);
  }),
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(() => Promise.resolve(() => {})) }));

const { flow, makeBackup, check, newBackup, importFiles } = await import("../settings/setup/importState.svelte");

describe("the import flow store", () => {
  beforeEach(() => {
    calls.length = 0;
    flow.replace = false;
    flow.again = false;
    flow.error = null;
  });

  it("a review after Replace was ticked for an earlier backup needs the tick again", async () => {
    flow.replace = true;
    await makeBackup("http://192.168.1.20:8089", "~/supernote-backup-2026-11-17-2");
    expect(flow.replace).toBe(false);
    flow.replace = true;
    await newBackup(new Date(2026, 10, 17, 9, 0));
    expect(flow.replace).toBe(false);
  });

  it("the files dry run never replaces, also in Import again", async () => {
    flow.replace = true;
    flow.again = true;
    await check();
    expect(calls.find((c) => c.command === "import_dry_run")?.args).toEqual({ replace: false, allowEmpty: false });
    // The import itself replaces.
    await importFiles(false);
    expect(calls.find((c) => c.command === "import_run")?.args).toEqual({ replace: true, allowEmpty: false });
  });

  it("a backup started in this run is this run's, also before it ends", async () => {
    await makeBackup("http://192.168.1.20:8089", "~/supernote-backup-2026-11-17");
    expect(flow.backup?.madeThisRun).toBe(true);
  });
});

describe("a refused backup", () => {
  it("leaves an earlier backup in the folder not this run's", async () => {
    const { invoke } = await import("@tauri-apps/api/core");
    vi.mocked(invoke).mockImplementation((command: string) => {
      if (command === "backup_start") return Promise.reject("The folder holds a finished backup. Choose an empty folder.");
      if (command === "backup_folder") return Promise.resolve({ state: "complete", time: new Date().toISOString() });
      return Promise.resolve(null);
    });
    await makeBackup("http://192.168.1.20:8089", "~/supernote-backup-2026-11-16");
    expect(flow.error).toBe("The folder holds a finished backup. Choose an empty folder.");
    expect(flow.backup?.state.state).toBe("complete");
    expect(flow.backup?.madeThisRun).toBe(false);
  });
});
