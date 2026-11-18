import { beforeEach, describe, expect, it, vi } from "vitest";
import type { NoteView } from "../lib/library-types";
import {
  ACTUAL,
  SPLIT_KEY,
  ZOOMS,
  clampSplit,
  keepPlace,
  readSplit,
  saveSplit,
  stepPage,
  viewerKey,
  zoomIn,
  zoomKey,
  zoomOut,
} from "../library/zoom";
import { pageAt } from "../library/review";

const calls: { command: string; args: Record<string, unknown> | undefined }[] = [];
const views: NoteView[] = [];

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn((command: string, args?: Record<string, unknown>) => {
    calls.push({ command, args });
    if (command === "note_open") return Promise.resolve(views.shift());
    // Page images wait: the tests only look at what was asked.
    if (command === "note_page_image") return new Promise(() => {});
    return Promise.resolve(null);
  }),
}));

const { ReviewSession } = await import("../library/session.svelte");

beforeEach(() => {
  calls.length = 0;
  views.length = 0;
});

function note(count: number): NoteView {
  return {
    key: "k1",
    path: "NOTE/Note/Plan.note",
    title: "Plan",
    catchingUp: false,
    pages: Array.from({ length: count }, (_, index) => ({
      index,
      pageId: `P${index}`,
      state: "done" as const,
      markdown: "",
      blocks: [],
      landscape: false,
    })),
  };
}

function asked(command: string): number[] {
  return calls.filter((c) => c.command === command).map((c) => c.args?.index as number);
}

async function opened(count: number) {
  views.push(note(count));
  const session = new ReviewSession("NOTE/Note/Plan.note");
  await session.open();
  return session;
}

/** A storage of its own, with the calls it got. */
function memory(initial: Record<string, string> = {}): Storage & { data: Record<string, string> } {
  const data = { ...initial };
  return {
    data,
    get length() {
      return Object.keys(data).length;
    },
    clear: () => {
      for (const key of Object.keys(data)) delete data[key];
    },
    getItem: (key: string) => (key in data ? data[key] : null),
    key: (index: number) => Object.keys(data)[index] ?? null,
    removeItem: (key: string) => {
      delete data[key];
    },
    setItem: (key: string, value: string) => {
      data[key] = value;
    },
  };
}

/** A storage whose every call throws, as a blocked one does. */
function broken(): Storage {
  const fail = () => {
    throw new Error("blocked");
  };
  return { length: 0, clear: fail, getItem: fail, key: fail, removeItem: fail, setItem: fail };
}

describe("zoom steps", () => {
  it("goes from fit width through 125, 150, 200 and 300 percent", () => {
    expect(ZOOMS).toEqual([1, 1.25, 1.5, 2, 3]);
    expect(ACTUAL).toBe(1);
    expect(zoomIn(1)).toBe(1.25);
    expect(zoomIn(1.25)).toBe(1.5);
    expect(zoomIn(1.5)).toBe(2);
    expect(zoomIn(2)).toBe(3);
    expect(zoomOut(3)).toBe(2);
    expect(zoomOut(2)).toBe(1.5);
    expect(zoomOut(1.25)).toBe(1);
  });

  it("stays at the ends", () => {
    expect(zoomIn(3)).toBe(3);
    expect(zoomOut(1)).toBe(1);
  });

  it("takes the next step from a value between two steps", () => {
    expect(zoomIn(1.1)).toBe(1.25);
    expect(zoomOut(1.1)).toBe(1);
    expect(zoomIn(5)).toBe(3);
    expect(zoomOut(0.5)).toBe(1);
  });
});

describe("zoomKey", () => {
  it("reads Cmd+Plus, Cmd+Minus and Cmd+0", () => {
    expect(zoomKey({ key: "=", metaKey: true })).toBe("in");
    expect(zoomKey({ key: "+", metaKey: true })).toBe("in");
    expect(zoomKey({ key: "-", metaKey: true })).toBe("out");
    expect(zoomKey({ key: "0", metaKey: true })).toBe("actual");
  });

  it("leaves every other key alone", () => {
    for (const key of ["=", "+", "-", "0"]) expect(zoomKey({ key, metaKey: false })).toBeNull();
    for (const key of ["1", "a", "Escape", "ArrowUp"]) expect(zoomKey({ key, metaKey: true })).toBeNull();
  });
});

describe("keepPlace", () => {
  it("keeps the same page at the same share after a zoom", () => {
    const before = { tops: [0, 100, 200, 300], end: 400 };
    const after = { tops: [0, 200, 400, 600], end: 800 };
    // A quarter into page 2.
    const top = keepPlace(225, before, after);
    expect(top).toBe(450);
    expect(pageAt(top, after.tops)).toBe(pageAt(225, before.tops));
    expect(keepPlace(0, before, after)).toBe(0);
    expect(keepPlace(350, before, after)).toBe(700);
  });

  it("is the top for no pages", () => {
    expect(keepPlace(50, { tops: [], end: 0 }, { tops: [], end: 0 })).toBe(0);
  });
});

describe("the split", () => {
  it("stays between a fifth and four fifths", () => {
    expect(clampSplit(0.5)).toBe(0.5);
    expect(clampSplit(0.1)).toBe(0.2);
    expect(clampSplit(0.95)).toBe(0.8);
    expect(clampSplit(Number.NaN)).toBe(0.5);
  });

  it("is read from the storage, 0.5 without one", () => {
    expect(SPLIT_KEY).toBe("notedown.review.split");
    expect(readSplit(memory({ [SPLIT_KEY]: "0.3" }))).toBe(0.3);
    expect(readSplit(memory({ [SPLIT_KEY]: "0.05" }))).toBe(0.2);
    expect(readSplit(memory())).toBe(0.5);
    expect(readSplit(memory({ [SPLIT_KEY]: "wide" }))).toBe(0.5);
    expect(readSplit(memory({ [SPLIT_KEY]: "" }))).toBe(0.5);
    expect(readSplit(null)).toBe(0.5);
    expect(readSplit(broken())).toBe(0.5);
  });

  it("is saved clamped, and a storage that fails is no error", () => {
    const storage = memory();
    saveSplit(storage, 0.35);
    expect(storage.data[SPLIT_KEY]).toBe("0.35");
    saveSplit(storage, 2);
    expect(readSplit(storage)).toBe(0.8);
    expect(() => saveSplit(broken(), 0.4)).not.toThrow();
    expect(() => saveSplit(null, 0.4)).not.toThrow();
  });
});

describe("the page viewer", () => {
  it("reads the keys that go to another page or close it", () => {
    const table: [string, "prev" | "next" | "close" | null][] = [
      ["ArrowLeft", "prev"],
      ["ArrowUp", "prev"],
      ["PageUp", "prev"],
      ["ArrowRight", "next"],
      ["ArrowDown", "next"],
      ["PageDown", "next"],
      ["Escape", "close"],
      ["Enter", null],
      [" ", null],
      ["Home", null],
      ["a", null],
    ];
    for (const [key, step] of table) expect(viewerKey(key), key).toBe(step);
  });

  it("steps to the page before or after, and stays at both ends", () => {
    expect(stepPage(3, 10, "prev")).toBe(2);
    expect(stepPage(3, 10, "next")).toBe(4);
    expect(stepPage(0, 10, "prev")).toBe(0);
    expect(stepPage(9, 10, "next")).toBe(9);
    expect(stepPage(0, 1, "next")).toBe(0);
    // An index past a note that got shorter comes back to its last page.
    expect(stepPage(12, 10, "prev")).toBe(9);
  });

  it("asks for the image of the page it shows and its neighbours", async () => {
    const session = await opened(12);
    session.seen(0, true);
    expect(asked("note_page_image")).toEqual([0, 1, 2]);
    session.focus(8);
    expect(session.focused).toBe(8);
    expect(asked("note_page_image")).toEqual([0, 1, 2, 7, 8, 9, 10]);
    // The pages far from it went: their calls end in Rust.
    expect(asked("note_page_drop")).toEqual([0, 1, 2]);
    session.focus(9);
    expect(asked("note_page_image")).toEqual([0, 1, 2, 7, 8, 9, 10, 11]);
    // Closed: the pages in view of the pages side count again.
    session.focus(null);
    expect(session.focused).toBeNull();
    expect(asked("note_page_image").slice(8)).toEqual([0, 1, 2]);
  });

  it("follows a reload that made the note shorter", async () => {
    const session = await opened(12);
    session.focus(11);
    views.push(note(5));
    await session.reload();
    expect(session.focused).toBe(4);
    views.push(note(0));
    await session.reload();
    expect(session.focused).toBeNull();
  });

  it("keeps a page within the note", async () => {
    const session = await opened(3);
    session.focus(7);
    expect(session.focused).toBe(2);
    session.focus(-1);
    expect(session.focused).toBe(0);
  });
});
