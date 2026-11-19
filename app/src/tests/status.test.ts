import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Status } from "../lib/api";

type Handler = (event: { payload: unknown }) => void;
const handlers = new Map<string, Handler>();
const unlisten = vi.fn();
let firstStatus: (value: Status) => void = () => {};

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (name: string, handler: Handler) => {
    handlers.set(name, handler);
    return unlisten;
  }),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(
    (command: string) =>
      new Promise((resolve, reject) => {
        if (command === "get_status") firstStatus = resolve as (value: Status) => void;
        else reject(new Error(`unexpected command ${command}`));
      }),
  ),
}));

const { status, startStatus } = await import("../lib/status.svelte");

function sample(ocr: string): Status {
  return {
    setup: { mode: "local", ready: true, line: null, note: null },
    server: { line: "Runs on this Mac", state: { state: "running", pid: 7, sinceMs: 1 }, deviceUrls: [] },
    pipeline: { line: "Runs", state: { state: "running", pid: 8, sinceMs: 1 } },
    device: { line: null, name: "Nomad", lastSyncMs: 5, syncing: false, online: true, filesChanged: null },
    ocr: { line: ocr, inWork: null, lastError: null, queued: 0, pending: 0, waiting: 0, lastRunMs: null, facts: true, tokenRefused: false, paused: false },
    agents: [],
    attention: [],
    existing: { server: true, pipeline: false },
    account: { imported: true, appDataImported: true },
    connection: "reached",
    firstSyncError: null,
  };
}

describe("the status store", () => {
  beforeEach(() => {
    handlers.clear();
    status.current = null;
  });

  it("the_store_applies_a_status_event", async () => {
    const stop = await startStatus();
    expect(handlers.has("status")).toBe(true);
    firstStatus(sample("Caught up"));
    await new Promise((r) => setTimeout(r, 0));
    expect(status.current?.ocr.line).toBe("Caught up");

    handlers.get("status")!({ payload: sample("3 pages waiting") });
    expect(status.current?.ocr.line).toBe("3 pages waiting");

    stop();
    expect(unlisten).toHaveBeenCalled();
  });

  it("an event wins over a slower first answer", async () => {
    await startStatus();
    handlers.get("status")!({ payload: sample("3 pages waiting") });
    firstStatus(sample("Caught up"));
    await new Promise((r) => setTimeout(r, 0));
    expect(status.current?.ocr.line).toBe("3 pages waiting");
  });
});
