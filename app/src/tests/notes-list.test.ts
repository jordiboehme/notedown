import { describe, expect, it, vi } from "vitest";
import { render } from "svelte/server";
import NotesList from "../library/NotesList.svelte";
import type { NoteSummary } from "../lib/library-types";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(() => Promise.resolve(null)) }));

const note: NoteSummary = {
  key: "A",
  path: "NOTE/Note/a.note",
  title: "a",
  pages: 1,
  changedMs: 0,
} as NoteSummary;

function html(notes: NoteSummary[] | null, onMarkAllSeen?: () => void) {
  return render(NotesList, {
    props: { title: "New notes", notes, empty: "No new notes.", onOpen: () => {}, onMarkAllSeen },
  }).body;
}

const button = (body: string) => body.match(/<button[^>]*class="mark[ "][^>]*>/)?.[0];

describe("the Mark All as Seen button", () => {
  it("is there only when the list is given the handler", () => {
    expect(button(html([note], () => {}))).toBeDefined();
    expect(html([note], () => {})).toContain("Mark All as Seen");
    expect(button(html([note]))).toBeUndefined();
    expect(html([note])).not.toContain("Mark All as Seen");
  });

  it("is disabled while there are no new notes or the list is loading", () => {
    expect(button(html([note], () => {}))).not.toContain("disabled");
    expect(button(html([], () => {}))).toContain("disabled");
    expect(button(html(null, () => {}))).toContain("disabled");
  });
});
