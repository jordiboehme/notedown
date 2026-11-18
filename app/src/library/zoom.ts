import { syncTop, type Offsets } from "./review";

// The zoom of the review's pages and the split between its two sides. Pure functions; the
// components hold the state.

/** The zoom steps of the pages side: 1 is the width of the pages column (fit width). */
export const ZOOMS = [1, 1.25, 1.5, 2, 3];
/** Actual Size: back to fit width. */
export const ACTUAL = 1;

/** The next larger step; the largest stays. */
export function zoomIn(z: number): number {
  return ZOOMS.find((step) => step > z) ?? ZOOMS[ZOOMS.length - 1];
}

/** The next smaller step; the smallest stays. */
export function zoomOut(z: number): number {
  return [...ZOOMS].reverse().find((step) => step < z) ?? ZOOMS[0];
}

/** Cmd+Plus (with or without Shift), Cmd+Minus and Cmd+0; null for any other key. */
export function zoomKey(e: { key: string; metaKey: boolean }): "in" | "out" | "actual" | null {
  if (!e.metaKey) return null;
  if (e.key === "=" || e.key === "+") return "in";
  if (e.key === "-") return "out";
  if (e.key === "0") return "actual";
  return null;
}

/** Where the pages side scrolls to after a zoom: the same page at the same share of it. */
export function keepPlace(scrollTop: number, before: Offsets, after: Offsets): number {
  return syncTop(scrollTop, before, after);
}

/** Where the split between the pages and the text is kept, per viewer. */
export const SPLIT_KEY = "notedown.review.split";
const SPLIT_DEFAULT = 0.5;

/** The share of the pages side, between a fifth and four fifths of the window. */
export function clampSplit(x: number): number {
  if (!Number.isFinite(x)) return SPLIT_DEFAULT;
  return Math.min(0.8, Math.max(0.2, x));
}

/** The split saved last; half and half without one, or when the storage cannot be read. */
export function readSplit(storage: Storage | null): number {
  try {
    const saved = storage?.getItem(SPLIT_KEY);
    if (saved === null || saved === undefined || saved.trim() === "") return SPLIT_DEFAULT;
    const x = Number(saved);
    return Number.isFinite(x) ? clampSplit(x) : SPLIT_DEFAULT;
  } catch {
    return SPLIT_DEFAULT;
  }
}

/** Saves the split; a storage that fails is no error (the split only stays for this window). */
export function saveSplit(storage: Storage | null, x: number): void {
  try {
    storage?.setItem(SPLIT_KEY, String(clampSplit(x)));
  } catch {
    // Not kept: the next window starts at half and half.
  }
}
