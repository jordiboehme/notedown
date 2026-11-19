import { describe, expect, it } from "vitest";
import type { ComponentProps } from "svelte";
import { render } from "svelte/server";
import type { Status } from "../lib/api";
import Panel from "../panel/Panel.svelte";
import { panelLines, pauseControl } from "../panel/lines";
import { newNotesView } from "../panel/notes";
import { notSetUp, sampleStatus } from "./sample";

// A fixed clock: nothing here waits or reads the time.
const now = Date.UTC(2026, 10, 16, 12, 0, 0);
const noop = () => {};

/** The status as Rust sends it while a pause holds: `line` says until when. */
function paused(line: string): Status {
  const status = sampleStatus(now);
  status.pipeline = { line, state: { state: "stopped" } };
  status.ocr = { ...status.ocr, line, paused: true };
  return status;
}

/** A pipeline the pause cannot act on: the status says that instead of the pause. */
function notOurs(state: "notConnected" | "outside"): Status {
  const status = sampleStatus(now);
  const line = state === "outside" ? "Notedown runs outside the app. Stop it there, then press Start." : "Not connected yet";
  status.pipeline = { line, state: state === "outside" ? { state, what: "pipeline" } : { state } };
  status.ocr = { ...status.ocr, line, paused: false };
  return status;
}

function ocrRow(status: Status) {
  return panelLines(status, now).find((l) => l.key === "ocr");
}

describe("pauseControl", () => {
  it("offers Pause OCR while the OCR is not paused", () => {
    expect(pauseControl(sampleStatus(now))).toEqual({ button: "Pause OCR", action: "menu", change: false });
    expect(ocrRow(sampleStatus(now))).toMatchObject({ text: "Caught up" });
  });

  it("offers Resume OCR Now and Change Pause while a timed pause holds", () => {
    const status = paused("OCR paused until 14:32");
    expect(pauseControl(status)).toEqual({ button: "Resume OCR Now", action: "resume", change: true });
    expect(ocrRow(status)).toMatchObject({ text: "OCR paused until 14:32", tone: "off", attention: false });
  });

  it("offers Resume OCR Now and Change Pause while a pause holds until resumed", () => {
    const status = paused("OCR paused");
    expect(pauseControl(status)).toEqual({ button: "Resume OCR Now", action: "resume", change: true });
    expect(ocrRow(status)).toMatchObject({ text: "OCR paused" });
  });

  it("offers nothing while the pipeline is not connected or another Notedown runs it", () => {
    // Their lines win over a pause, as in Rust's status_from: the pause cannot act there.
    const notConnected = notOurs("notConnected");
    expect(pauseControl(notConnected)).toBeNull();
    expect(ocrRow(notConnected)).toMatchObject({ text: "Not connected yet" });
    const outside = notOurs("outside");
    expect(pauseControl(outside)).toBeNull();
    expect(ocrRow(outside)).toMatchObject({ text: "Notedown runs outside the app. Stop it there, then press Start." });
  });

  it("offers nothing before the first status or the setup", () => {
    expect(pauseControl(null)).toBeNull();
    expect(pauseControl(notSetUp())).toBeNull();
  });

  it("sits on the OCR row only", () => {
    const lines = panelLines(paused("OCR paused"), now);
    expect(lines.filter((l) => l.pause !== null).map((l) => l.key)).toEqual(["ocr"]);
    expect(ocrRow(paused("OCR paused"))?.pause).toEqual(pauseControl(paused("OCR paused")));
  });
});

describe("the panel's OCR pause", () => {
  const actions = {
    onSetUp: noop,
    onOpenLibrary: noop,
    onSettings: noop,
    onQuit: noop,
    onConfirmQuit: noop,
    onCancelQuit: noop,
    onOpenNote: noop,
    onMoreNotes: noop,
    onPauseMenu: noop,
    onResumeOcr: noop,
  };

  function html(props: Partial<ComponentProps<typeof Panel>>) {
    return render(Panel, { props: { status: sampleStatus(now), now, asking: false, notes: newNotesView(null), ...actions, ...props } }).body;
  }

  it("shows Pause OCR in the OCR row", () => {
    const body = html({});
    expect(body).toContain(">Pause OCR</button>");
    expect(body).toContain('aria-haspopup="menu"');
    expect(body).not.toContain("Resume OCR Now");
    expect(body).not.toContain("Change Pause");
  });

  it("shows the pause line, Resume OCR Now and Change Pause while paused", () => {
    const body = html({ status: paused("OCR paused until 14:32") });
    expect(body).toContain("OCR paused until 14:32");
    expect(body).toContain(">Resume OCR Now</button>");
    expect(body).toContain(">Change Pause</button>");
    expect(body).not.toContain(">Pause OCR</button>");
  });

  it("shows no pause control while another Notedown runs the OCR", () => {
    const body = html({ status: notOurs("outside") });
    expect(body).not.toContain("Pause OCR");
    expect(body).not.toContain("Resume OCR Now");
  });

  it("uses no em or en dash", () => {
    expect(html({})).not.toMatch(/[–—]/);
    expect(html({ status: paused("OCR paused") })).not.toMatch(/[–—]/);
  });
});
