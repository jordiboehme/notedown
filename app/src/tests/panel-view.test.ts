import { describe, expect, it } from "vitest";
import type { ComponentProps } from "svelte";
import { render } from "svelte/server";
import Panel from "../panel/Panel.svelte";
import { newNotesView } from "../panel/notes";
import type { NoteSummary } from "../lib/library-types";
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
  onOpenNote: noop,
  onMoreNotes: noop,
};

function note(i: number, title = `Note ${i}`): NoteSummary {
  return { key: `k${i}`, path: `NOTE/Note/Work/${title}.note`, title, pages: 3, pagesWithText: 2, changedMs: now - i * 60_000 };
}

function html(props: Partial<ComponentProps<typeof Panel>>) {
  return render(Panel, { props: { status: sampleStatus(now), now, asking: false, notes: newNotesView(null), ...actions, ...props } }).body;
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

describe("newNotesView", () => {
  it("shows up to five new notes, then how many more there are", () => {
    const notes = Array.from({ length: 8 }, (_, i) => note(i + 1));
    const view = newNotesView(notes);
    expect(view.rows.map((r) => r.title)).toEqual(["Note 1", "Note 2", "Note 3", "Note 4", "Note 5"]);
    expect(view.rows[0]).toEqual({ key: "k1", path: "NOTE/Note/Work/Note 1.note", title: "Note 1", folder: "Note / Work" });
    expect(view.more).toBe("3 more");
  });

  it("says no more for five or fewer, and nothing before the notes are read", () => {
    expect(newNotesView(Array.from({ length: 5 }, (_, i) => note(i))).more).toBeNull();
    expect(newNotesView([note(1)]).rows).toHaveLength(1);
    expect(newNotesView([])).toEqual({ rows: [], more: null });
    expect(newNotesView(null)).toEqual({ rows: [], more: null });
  });

  it("counts many more with a separator", () => {
    expect(newNotesView(Array.from({ length: 1239 }, (_, i) => note(i))).more).toBe("1,234 more");
  });
});

describe("the panel's new notes", () => {
  it("lists the new notes as buttons with their titles as text", () => {
    const notes = [note(1, "<b>Plan</b>"), ...Array.from({ length: 6 }, (_, i) => note(i + 2))];
    const body = html({ notes: newNotesView(notes) });
    expect(body).toContain("New notes");
    expect(body).toContain("&lt;b>Plan&lt;/b>");
    expect(body).not.toContain("<b>Plan</b>");
    expect(body).toContain(">2 more</button>");
    expect(body).not.toMatch(/[\u2013\u2014]/);
  });

  it("shows nothing for no new notes", () => {
    expect(html({ notes: newNotesView([]) })).not.toContain("New notes</");
  });
});
