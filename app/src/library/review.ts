import { invoke } from "@tauri-apps/api/core";
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

/** Where a search hit is: the page by its id and the block in it. */
export type BlockAt = { pageId: string; blockId: string };

export type ReviewPage = {
  index: number;
  /** The page's id in the note: a search hit names its page by it. */
  pageId: string;
  label: string;
  state: PageState;
  /** The page's text as the pipeline wrote it; shown as text, never as HTML. */
  markdown: string;
  /** A plain line about the page when it has no text to show; null when it has. */
  stateText: string | null;
  /** Width by height, for the placeholder of its image. */
  ratio: number;
  /** The page's text block by block, in their order; together they are `markdown`. */
  blocks: { id: string; text: string }[];
};

const STATE_TEXTS: Record<Exclude<PageState, "done">, string> = {
  pending: "Waiting for the OCR.",
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
    pageId: page.pageId,
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
    blocks: page.blocks.map((block) => ({ id: block.id, text: block.text })),
  }));
}

/**
 * The row of the page `at` names and its block: the page alone (`blockId` null) when the block
 * is gone, nothing when the page is gone.
 */
export function findBlock(rows: ReviewPage[], at: BlockAt): { index: number; blockId: string | null } | null {
  const row = rows.find((r) => r.pageId === at.pageId);
  if (!row) return null;
  return { index: row.index, blockId: row.blocks.some((b) => b.id === at.blockId) ? at.blockId : null };
}

/**
 * The line above the review while the note is not read to the end. `pause` is the status
 * line's pause text ("OCR paused until 14:32") while an OCR pause holds: then nothing reads the
 * note, and the review says so in the same words.
 */
export function reviewNotice(view: NoteView, pause: string | null = null): string | null {
  if (!view.catchingUp) return null;
  return pause ?? "Notedown is still reading this note.";
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

/**
 * Whether a `NOTES_EVENT` concerns the review of the note at `path`: its payload names the
 * notes whose store file changed. An event without a readable payload counts, so a review
 * never misses its text.
 */
export function concernsNote(payload: unknown, path: string): boolean {
  const paths = (payload as { paths?: unknown } | null | undefined)?.paths;
  if (!Array.isArray(paths)) return true;
  return paths.includes(path);
}

/** The name of a note by its path, as the review shows it before or without its text. */
export { noteTitle } from "./view";

export type Side = "pages" | "text";

/**
 * Keeps the two sides from following each other back: the window remembers the value it set
 * on a side, and the scroll event of that side within a pixel of it is not followed. Any other
 * scroll of the side is the user's and ends the guard.
 */
export function syncGuard() {
  const set: Partial<Record<Side, number>> = {};
  return {
    set(side: Side, top: number) {
      set[side] = top;
    },
    follows(side: Side, top: number): boolean {
      const expected = set[side];
      delete set[side];
      return expected === undefined || Math.abs(top - expected) > 1;
    },
  };
}

/**
 * Whether the mark of a search hit stays: the text side is still where the window scrolled it
 * (`markedTop`), within a pixel. Any other scroll is the user's, whatever moved it.
 */
export function keepsHit(markedTop: number, scrollTop: number): boolean {
  return Math.abs(scrollTop - markedTop) <= 1;
}

/** Save Markdown can save the note: some page has text. */
export function canSaveMarkdown(view: NoteView | null): boolean {
  return view !== null && view.pages.some((page) => page.markdown.trim() !== "");
}

/**
 * The context menu of the note at `path` (a row of a notes list, a .note file): Rust builds
 * it ("Save Markdown...", enabled when the note has text) and does its work.
 */
export function noteMenu(event: { preventDefault(): void }, path: string) {
  event.preventDefault();
  invoke("library_menu", { target: { kind: "note", path } }).catch((error: unknown) =>
    console.error("notedown: library_menu failed", error),
  );
}
