import { describe, expect, it } from "vitest";
import type { ComponentProps } from "svelte";
import { render } from "svelte/server";
import Panel from "../panel/Panel.svelte";
import { notSetUp, sampleStatus } from "./sample";

const now = Date.UTC(2026, 10, 16, 12, 0, 0);
const noop = () => {};
const actions = {
  onSetUp: noop,
  onOpenLibrary: noop,
  onSettings: noop,
  onQuit: noop,
  onConfirmQuit: noop,
  onCancelQuit: noop,
};

function html(props: Partial<ComponentProps<typeof Panel>>) {
  return render(Panel, { props: { status: sampleStatus(now), now, asking: false, ...actions, ...props } }).body;
}

describe("the panel", () => {
  it("shows the lines, the slot for new notes and the footer", () => {
    const body = html({});
    expect(body).toContain("Supernote");
    expect(body).toContain("12 min ago");
    expect(body).toContain("3 notes behind");
    expect(body).toContain("Caught up");
    expect(body).toContain('data-slot="new-notes"');
    for (const button of ["Open Library", "Settings", "Quit"]) expect(body).toContain(`>${button}</button>`);
    expect(body).not.toContain("Set up");
  });

  it("not set up shows the line and Set up", () => {
    const body = html({ status: notSetUp() });
    expect(body).toContain("Notedown is not set up yet");
    expect(body).toContain(">Set up</button>");
    expect(body).not.toContain("Caught up");
  });

  it("waits for the first status", () => {
    expect(html({ status: null })).toContain("Looking at the status...");
  });

  it("puts the Quit question with Quit and Cancel", () => {
    const body = html({ asking: true });
    expect(body).toContain("The Supernote cannot sync while Notedown is not running. Quit anyway?");
    expect(body).toContain(">Cancel</button>");
    expect(body).toContain('role="alertdialog"');
  });

  it("marks an attention line", () => {
    const status = sampleStatus(now);
    status.attention = ["ocrError"];
    status.ocr.lastError = { text: "no model", atMs: now - 60_000 };
    status.ocr.line = "The last OCR pass failed: no model";
    expect(html({ status })).toContain('data-attention="true"');
  });

  it("uses no em or en dash", () => {
    expect(html({ asking: true })).not.toMatch(/[\u2013\u2014]/);
    expect(html({ status: notSetUp() })).not.toMatch(/[\u2013\u2014]/);
  });
});
