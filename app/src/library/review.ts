import type { NoteView } from "../lib/library-types";

// The view models of the review: a note's handwritten pages on one side and its Markdown on
// the other, scrolled together page by page. Pure functions; review.svelte.ts holds the state.

/** Width by height of a Supernote page upright (1404 x 1872): the placeholder before an image. */
export const PORTRAIT = 1404 / 1872;

/** Pages asked for above the view and below it. */
export const PAGES_BEHIND = 1;
export const PAGES_AHEAD = 2;
/** Pages kept on each side of the view; images farther away are dropped. */
export const PAGES_KEPT = 4;

export type PageState = NoteView["pages"][number]["state"];

export type ReviewPage = {
  index: number;
  label: string;
  state: PageState;
  /** The page's text as the pipeline wrote it; shown as text, never as HTML. */
  markdown: string;
  /** A plain line about the page when it has no text to show; null when it has. */
  stateText: string | null;
  /** Width by height, for the placeholder of its image. */
  ratio: number;
};

const STATE_TEXTS: Record<Exclude<PageState, "done">, string> = {
  pending: "Not read yet.",
  failed: "Notedown could not read this page.",
  truncated: "The text of this page was cut short.",
};

/**
 * The pages of the review. `ratios` holds the width by height of the images loaded so far (by
 * page index); a page without one is landscape when the view says so, else portrait.
 */
export function reviewPages(view: NoteView, ratios: Record<number, number> = {}): ReviewPage[] {
  return view.pages.map((page) => ({
    index: page.index,
    label: `Page ${page.index + 1}`,
    state: page.state,
    markdown: page.markdown,
    stateText:
      page.state === "done"
        ? page.markdown.trim() === ""
          ? "No text on this page."
          : null
        : STATE_TEXTS[page.state],
    ratio: ratios[page.index] ?? (page.landscape ? 1 / PORTRAIT : PORTRAIT),
  }));
}

/** The line above the review while the note is not read to the end. */
export function reviewNotice(view: NoteView): string | null {
  return view.catchingUp ? "Notedown is still reading this note." : null;
}

/** The page at `scrollTop`, given the tops of the pages (ascending): the last one at or above it. */
export function pageAt(scrollTop: number, offsets: number[]): number {
  let page = 0;
  for (let i = 1; i < offsets.length; i++) {
    if (offsets[i] > scrollTop) break;
    page = i;
  }
  return page;
}

/** The tops of the pages of one side and where its last page ends. */
export type Offsets = { tops: number[]; end: number };

/**
 * Where the other side scrolls to when one side is at `scrollTop`: the same page, at the same
 * share of it.
 */
export function syncTop(scrollTop: number, from: Offsets, to: Offsets): number {
  if (from.tops.length === 0 || to.tops.length === 0) return 0;
  const page = Math.min(pageAt(scrollTop, from.tops), to.tops.length - 1);
  const top = from.tops[page];
  const bottom = from.tops[page + 1] ?? from.end;
  const share = bottom > top ? Math.min(1, Math.max(0, (scrollTop - top) / (bottom - top))) : 0;
  const toTop = to.tops[page];
  const toBottom = to.tops[page + 1] ?? to.end;
  return toTop + share * (toBottom - toTop);
}

/**
 * Which pages want their image while `visible` pages are in view: `near` (one above, two
 * below) are asked for, those outside `keep` (four on each side) are dropped. Both are ranges
 * of indexes, ends included. Null when no page is in view (nothing changes then).
 */
export function nearPages(
  visible: Iterable<number>,
  count: number,
): { near: [number, number]; keep: [number, number] } | null {
  const shown = [...visible];
  if (shown.length === 0 || count === 0) return null;
  const first = Math.min(...shown);
  const last = Math.max(...shown);
  const clamp = (i: number) => Math.min(count - 1, Math.max(0, i));
  return {
    near: [clamp(first - PAGES_BEHIND), clamp(last + PAGES_AHEAD)],
    keep: [clamp(first - PAGES_KEPT), clamp(last + PAGES_KEPT)],
  };
}
