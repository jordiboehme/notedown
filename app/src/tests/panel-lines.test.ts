import { describe, expect, it } from "vitest";
import { needsQuitWarning, panelLines } from "../panel/lines";
import type { ChildState, Mode } from "../lib/api";
import { notSetUp, sampleStatus } from "./sample";

const now = Date.UTC(2026, 10, 16, 12, 0, 0);

describe("panelLines", () => {
  it("not set up gives one line and Set up", () => {
    const lines = panelLines(notSetUp(), now);
    expect(lines).toHaveLength(1);
    expect(lines[0]).toMatchObject({
      key: "setup",
      text: "Notedown is not set up yet",
      action: "setup",
      attention: false,
    });
  });

  it("a reset of the settings is said above the setup line", () => {
    const status = notSetUp();
    status.setup.note = "The settings file could not be read. It was set aside and the settings were reset.";
    const lines = panelLines(status, now);
    expect(lines.map((l) => l.key)).toEqual(["note", "setup"]);
    expect(lines[0].text).toBe(status.setup.note);
  });

  it("a full status gives the Supernote, agent and OCR lines with relative times", () => {
    const lines = panelLines(sampleStatus(now), now);
    expect(lines.map((l) => l.key)).toEqual(["supernote", "agent:Claude", "ocr"]);
    expect(lines[0]).toMatchObject({ label: "Supernote", text: "Synced", when: "12 min ago", tone: "ok" });
    expect(lines[1]).toMatchObject({ label: "Claude", text: "3 notes behind", when: "2 h ago", tone: "wait" });
    expect(lines[2]).toMatchObject({ label: "OCR", text: "Caught up", when: null, more: null, tone: "ok" });
    expect(lines.every((l) => !l.attention && l.action === null)).toBe(true);
  });

  it("says the device syncs now, an agent that never read and the page in work", () => {
    const status = sampleStatus(now);
    status.device.syncing = true;
    status.agents[0] = { ...status.agents[0], readAtMs: null, notesBehind: 0, behind: "Up to date" };
    status.ocr = {
      ...status.ocr,
      line: "4 pages waiting",
      queued: 3,
      pending: 1,
      inWork: { note: "Meeting", path: "Note/Meeting.note", page: 2, pages: 5, startedMs: now - 5_000 },
    };
    const lines = panelLines(status, now);
    expect(lines[0]).toMatchObject({ text: "Syncing now", when: "12 min ago" });
    expect(lines[1]).toMatchObject({ text: "Up to date", when: "Not read yet", tone: "ok" });
    expect(lines[2]).toMatchObject({ text: "4 pages waiting", more: "Meeting, page 2 of 5", tone: "wait" });
  });

  it("shows the line of a device that has not synced as it is", () => {
    const status = sampleStatus(now);
    status.device = { line: "No Supernote yet", name: null, lastSyncMs: null, syncing: false, online: false, filesChanged: null };
    expect(panelLines(status, now)[0]).toMatchObject({ text: "No Supernote yet", when: null, tone: "off" });
  });

  it("attention lines come first", () => {
    const status = sampleStatus(now);
    status.ocr = {
      ...status.ocr,
      line: "The last OCR pass failed: no model",
      lastError: { text: "no model", atMs: now - 3 * 60_000 },
    };
    status.device.lastSyncMs = now - 3 * 86_400_000;
    status.attention = ["ocrError"];
    let lines = panelLines(status, now);
    expect(lines.map((l) => l.key)).toEqual(["ocr", "supernote", "agent:Claude"]);
    expect(lines[0]).toMatchObject({ attention: true, tone: "problem", when: "3 min ago" });

    status.attention = ["ocrError", "tokenRefused"];
    status.device = { line: "The server refused the login", name: null, lastSyncMs: null, syncing: false, online: false, filesChanged: null };
    lines = panelLines(status, now);
    expect(lines.map((l) => l.key)).toEqual(["supernote", "ocr", "agent:Claude"]);
    expect(lines[0]).toMatchObject({ attention: true, tone: "problem" });
  });

  it("shows the server while it does not run or does not answer", () => {
    const status = sampleStatus(now);
    status.server = { ...status.server, line: "Notedown runs outside the app. Stop it there, then press Start.", state: { state: "outside", what: "server" } };
    let lines = panelLines(status, now);
    expect(lines[0]).toMatchObject({ key: "server", label: "Server", text: status.server.line, attention: false });

    status.server = { ...status.server, line: "Runs on this Mac but does not answer", state: { state: "running", pid: 7, sinceMs: 1 } };
    status.attention = ["serverUnreachable"];
    lines = panelLines(status, now);
    expect(lines[0]).toMatchObject({ key: "server", attention: true, tone: "problem" });

    const remote = sampleStatus(now);
    remote.setup.mode = "remote";
    remote.server = { line: "Runs on https://notes.example", state: null, deviceUrls: [] };
    expect(panelLines(remote, now).some((l) => l.key === "server")).toBe(false);
    remote.server.line = "https://notes.example does not answer";
    expect(panelLines(remote, now).some((l) => l.key === "server")).toBe(true);
  });

  it("says Stopping... while a supervisor stops", () => {
    const status = sampleStatus(now);
    status.server = { ...status.server, line: "Stopping...", state: { state: "stopping" } };
    status.pipeline = { line: "Stopping...", state: { state: "stopping" } };
    const lines = panelLines(status, now);
    expect(lines.find((l) => l.key === "server")).toMatchObject({ text: "Stopping...", tone: "wait" });
    expect(lines.find((l) => l.key === "ocr")).toMatchObject({ text: "Stopping...", tone: "wait" });
  });

  it("a pipeline that failed and waits to start again is attention on the OCR line", () => {
    const status = sampleStatus(now);
    const line = "Stopped with an error: boom. It starts again soon.";
    status.pipeline = { line, state: { state: "backoff", untilMs: now + 4_000, lastError: "boom" } };
    status.attention = ["pipelineBackoff"];
    const lines = panelLines(status, now);
    expect(lines[0]).toMatchObject({ key: "ocr", text: line, attention: true, tone: "problem" });
  });

  it("a missing account is a setup line above the rest", () => {
    const status = sampleStatus(now);
    status.setup = { mode: "local", ready: false, line: "Notedown has no account yet", note: null };
    status.ocr.line = "OCR starts when there is an account";
    status.agents = [];
    const lines = panelLines(status, now);
    expect(lines.map((l) => l.key)).toEqual(["setup", "supernote", "ocr"]);
    expect(lines[0]).toMatchObject({ text: "Notedown has no account yet", action: "setup" });
    expect(lines[2]).toMatchObject({ tone: "off" });
  });

  it("uses no em or en dash", () => {
    const status = sampleStatus(now);
    status.ocr.inWork = { note: "A", path: "A.note", page: 1, pages: 2, startedMs: now };
    const text = JSON.stringify(panelLines(status, now));
    expect(text).not.toMatch(/[\u2013\u2014]/);
  });
});

describe("needsQuitWarning", () => {
  const running: ChildState = { state: "running", pid: 7, sinceMs: 1 };
  const withServer = (mode: Mode, state: ChildState | null) => {
    const status = sampleStatus(now);
    status.setup.mode = mode;
    status.server.state = state;
    return status;
  };

  it("only in Local mode with the server running", () => {
    expect(needsQuitWarning(withServer("local", running))).toBe(true);
    expect(needsQuitWarning(withServer("remote", null))).toBe(false);
    expect(needsQuitWarning(withServer("notSet", null))).toBe(false);
    expect(needsQuitWarning(null)).toBe(false);
    const others: ChildState[] = [
      { state: "stopped" },
      { state: "notConnected" },
      { state: "starting" },
      { state: "stopping" },
      { state: "backoff", untilMs: 5, lastError: "boom" },
      { state: "outside", what: "server" },
    ];
    for (const state of others) expect(needsQuitWarning(withServer("local", state)), state.state).toBe(false);
  });
});
