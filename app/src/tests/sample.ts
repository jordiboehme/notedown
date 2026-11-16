import type { Status } from "../lib/api";

/** A Local setup where everything runs, as a base for the tests to change. */
export function sampleStatus(now: number): Status {
  return {
    setup: { mode: "local", ready: true, line: null, note: null },
    server: { line: "Runs on this Mac", state: { state: "running", pid: 7, sinceMs: 1 }, deviceUrls: [] },
    pipeline: { line: "Runs", state: { state: "running", pid: 8, sinceMs: 1 } },
    device: { line: null, name: "Nomad", lastSyncMs: now - 12 * 60_000, syncing: false, online: true },
    ocr: { line: "Caught up", inWork: null, lastError: null, queued: 0, pending: 0, waiting: 0 },
    agents: [
      {
        name: "Claude",
        readAtMs: now - 2 * 3_600_000,
        ackedAtMs: now - 2 * 3_600_000,
        notesBehind: 3,
        more: false,
        behind: "3 notes behind",
      },
    ],
    attention: [],
    existing: { server: true, pipeline: true },
  };
}

/** The sample before any setup, as Rust sends it. */
export function notSetUp(): Status {
  return {
    setup: { mode: "notSet", ready: false, line: "Notedown is not set up yet", note: null },
    server: { line: "Not set up yet", state: null, deviceUrls: [] },
    pipeline: { line: "Not set up yet", state: { state: "stopped" } },
    device: { line: "Not set up yet", name: null, lastSyncMs: null, syncing: false, online: false },
    ocr: { line: "Not set up yet", inWork: null, lastError: null, queued: 0, pending: 0, waiting: 0 },
    agents: [],
    attention: [],
    existing: { server: false, pipeline: false },
  };
}
