import type { Attention, ChildState, Status } from "../lib/api";
import { relativeTime } from "../lib/format";

/** The colour of a line's dot: runs, waits or works, off or not there yet, a problem. */
export type Tone = "ok" | "wait" | "off" | "problem";

/** One line of the panel: a label with a time on the right, and the text below. */
export interface PanelLine {
  key: string;
  label: string;
  text: string;
  /** When it happened, said relative to now. */
  when: string | null;
  /** A second, smaller line (the page in work). */
  more: string | null;
  tone: Tone;
  /** Part of what lights the dot of the tray icon; such lines come first. */
  attention: boolean;
  /** The line has a button: "setup" opens Settings at Setup. */
  action: "setup" | null;
}

function line(key: string, label: string, text: string, rest: Partial<PanelLine> = {}): PanelLine {
  return { key, label, text, when: null, more: null, tone: "off", attention: false, action: null, ...rest };
}

function has(status: Status, ...kinds: Attention[]): boolean {
  return kinds.some((kind) => status.attention.includes(kind));
}

function childTone(state: ChildState): Tone {
  switch (state.state) {
    case "running":
      return "ok";
    case "starting":
    case "stopping":
      return "wait";
    case "backoff":
      return "problem";
    default:
      return "off";
  }
}

/** The settings note and the setup line, when there is one. */
function setupLines(status: Status): PanelLine[] {
  const lines: PanelLine[] = [];
  if (status.setup.note) lines.push(line("note", "Settings", status.setup.note, { tone: "wait" }));
  if (status.setup.mode === "notSet" || !status.setup.ready) {
    const text = status.setup.line ?? "Notedown is not set up yet";
    lines.push(line("setup", "Setup", text, { action: "setup" }));
  }
  return lines;
}

/** The server only when it does not simply run: the panel stays short while all is well. */
function serverLine(status: Status): PanelLine | null {
  const { line: text, state } = status.server;
  const attention = has(status, "serverUnreachable", "serverBackoff");
  const quiet = state ? state.state === "running" : text.startsWith("Runs on ");
  if (quiet && !attention) return null;
  const tone = attention ? "problem" : state ? childTone(state) : "off";
  return line("server", "Server", text, { tone, attention });
}

function supernoteLine(status: Status, now: number): PanelLine {
  const device = status.device;
  const attention = has(status, "tokenRefused", "oldSync");
  if (device.line !== null) {
    return line("supernote", "Supernote", device.line, { tone: attention ? "problem" : "off", attention });
  }
  return line("supernote", "Supernote", device.syncing ? "Syncing now" : "Synced", {
    when: device.lastSyncMs === null ? null : relativeTime(device.lastSyncMs, now),
    tone: attention ? "problem" : "ok",
    attention,
  });
}

function agentLines(status: Status, now: number): PanelLine[] {
  return status.agents.map((agent) =>
    line(`agent:${agent.name}`, agent.name, agent.behind, {
      when: agent.readAtMs === null ? "Not read yet" : relativeTime(agent.readAtMs, now),
      tone: agent.notesBehind > 0 || agent.more ? "wait" : "ok",
    }),
  );
}

/** The OCR queue, or the state of the pipeline while it does not run. */
function ocrLine(status: Status, now: number): PanelLine {
  const ocr = status.ocr;
  const attention = has(status, "ocrError", "pipelineBackoff");
  const pipeline = status.pipeline.state;
  if (pipeline.state !== "running" && pipeline.state !== "notConnected") {
    return line("ocr", "OCR", status.pipeline.line, {
      tone: attention ? "problem" : childTone(pipeline),
      attention,
    });
  }
  const work = ocr.inWork;
  let tone: Tone = "off";
  if (ocr.lastError) tone = "problem";
  else if (ocr.queued + ocr.pending + ocr.waiting > 0) tone = "wait";
  else if (ocr.line === "Caught up") tone = "ok";
  return line("ocr", "OCR", ocr.line, {
    when: ocr.lastError ? relativeTime(ocr.lastError.atMs, now) : null,
    more: work ? `${work.note}, page ${work.page} of ${work.pages}` : null,
    tone: attention ? "problem" : tone,
    attention,
  });
}

/**
 * The lines of the panel (mockup B): the Supernote, the agents and OCR, the server only when
 * it does not run, and the setup line while something is missing. Lines that light the dot of
 * the tray icon come first; the order of the rest stays. `now` is in ms since the epoch.
 */
export function panelLines(status: Status, now: number): PanelLine[] {
  const setup = setupLines(status);
  if (status.setup.mode === "notSet") return setup;
  const server = serverLine(status);
  const lines = [
    ...setup,
    ...(server ? [server] : []),
    supernoteLine(status, now),
    ...agentLines(status, now),
    ocrLine(status, now),
  ];
  return [...lines.filter((l) => l.attention), ...lines.filter((l) => !l.attention)];
}

/**
 * Whether Quit asks first: only in Local mode while the app's own server runs, because the
 * Supernote cannot sync once it stops. Rust's `quit_needs_question` keeps the same rule.
 */
export function needsQuitWarning(status: Status | null): boolean {
  return status?.setup.mode === "local" && status.server.state?.state === "running";
}
