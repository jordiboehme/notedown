import { describe, expect, it } from "vitest";
import {
  ACTUAL,
  SPLIT_KEY,
  ZOOMS,
  clampSplit,
  keepPlace,
  readSplit,
  saveSplit,
  zoomIn,
  zoomKey,
  zoomOut,
} from "../library/zoom";
import { pageAt } from "../library/review";

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
