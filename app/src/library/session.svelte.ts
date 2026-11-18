import { invoke } from "@tauri-apps/api/core";
import type { NoteView } from "../lib/library-types";
import { findBlock, nearPages, reviewPages, type BlockAt } from "./review";

/** The image of a page: asked for, shown (a data URL), or why there is none. */
export type PageImage = { state: "loading" } | { state: "shown"; url: string } | { state: "failed"; text: string };

/** What Rust answers to a page whose call was dropped (`note_page_drop`). */
const DROPPED = "The page is not shown anymore.";
/** What Rust answers to a page call that the window ended itself: no failure, the placeholder stays. */
const ENDED = new Set([DROPPED, "The review was closed."]);

function text(error: unknown): string {
  return typeof error === "string" ? error : error instanceof Error ? error.message : String(error);
}

/** The start of the line above a view whose reload failed. */
const STALE = "This view is not up to date.";

/** The text of a note, page by page: a reload that changes it brought new text. */
function textOf(view: NoteView): string {
  return JSON.stringify(view.pages.map((page) => [page.pageId, page.markdown]));
}

/**
 * One review of the note at `path`: its text from `note_open`, and the images of the pages
 * near the view. The window tells it which pages are in view (`seen`); it asks Rust for the
 * near ones and drops the far ones. `reload` reads the note again when the store changed.
 * `close` ends it in Rust (`note_close`).
 */
export class ReviewSession {
  readonly path: string;
  view = $state<NoteView | null>(null);
  error = $state<string | null>(null);
  /**
   * Why the last reload failed while the old view stays (a gone note says so); null after a
   * reload that worked.
   */
  stale = $state<string | null>(null);
  /** The images by page index; a page without one shows its placeholder. */
  images = $state<Record<number, PageImage>>({});
  /** Width by height of the images loaded so far, by page index; kept after a drop. */
  ratios = $state<Record<number, number>>({});
  /** The page shown alone in the large view; null while there is none. */
  focused = $state<number | null>(null);
  /**
   * Where the text side scrolls to once: the block of a search hit (`blockId` null when only
   * its page is left), set by the first read that works. The window sets it back to null.
   */
  target = $state<{ index: number; blockId: string | null } | null>(null);

  #visible = new Set<number>();
  /** The `note_open` that runs, and whether another one was asked for meanwhile. */
  #loading: Promise<void> | null = null;
  #again = false;
  /** Counts the asks of each page: an answer of an older ask (dropped since) is not shown. */
  #asks = new Map<number, number>();
  #closed = false;

  /** The block the review opens at; used by the first read that works, then gone. */
  #at: BlockAt | null;

  constructor(path: string, at: BlockAt | null = null) {
    this.path = path;
    this.#at = at;
  }

  /** Reads the note; opening it marks its text seen (when the store knows the note). */
  open(): Promise<void> {
    return this.#load();
  }

  /**
   * Reads the note again (the store changed): the new view replaces the old one. Images stay
   * for the pages whose page id at their index is the same; the others go. New text is marked
   * seen. On an error the old view stays. A reload asked for while one runs gives one more
   * after it, never two at once, so an older answer never replaces a newer one.
   */
  reload(): Promise<void> {
    return this.#load();
  }

  #load(): Promise<void> {
    if (this.#closed) return Promise.resolve();
    if (this.#loading) {
      this.#again = true;
      return this.#loading;
    }
    this.#loading = (async () => {
      try {
        do {
          this.#again = false;
          await this.#read();
        } while (this.#again && !this.#closed);
      } finally {
        this.#loading = null;
      }
    })();
    return this.#loading;
  }

  async #read(): Promise<void> {
    let view: NoteView;
    try {
      view = await invoke<NoteView>("note_open", { path: this.path });
    } catch (error) {
      if (this.#closed) return;
      const why = text(error);
      // A reload that fails keeps what is shown and says why above it.
      if (!this.view) this.error = why;
      else if (!ENDED.has(why)) this.stale = `${STALE} ${why}`;
      return;
    }
    if (this.#closed) return;
    const old = this.view;
    this.view = view;
    this.error = null;
    this.stale = null;
    if (old) this.#keepImages(old, view);
    if (this.#at) {
      this.target = findBlock(reviewPages(view), this.#at);
      this.#at = null;
    }
    // A page shown alone that is not in the note anymore: its last page, or no view.
    if (this.focused !== null && this.focused >= view.pages.length) {
      this.focused = view.pages.length > 0 ? view.pages.length - 1 : null;
    }
    if (view.key !== null && (old === null || old.key === null || textOf(old) !== textOf(view))) {
      invoke("note_seen", { key: view.key }).catch((error: unknown) =>
        console.error("notedown: note_seen failed", error),
      );
    }
    this.#update();
  }

  /**
   * Drops the images (and ratios) of the indexes whose page is another one now, or gone. When
   * the file is another version (its md5 changed), every image goes: the strokes may differ.
   * The ratio of a page that stayed is kept.
   */
  #keepImages(old: NoteView, view: NoteView) {
    const newFile = old.md5 !== view.md5;
    const indexes = new Set([...Object.keys(this.images), ...Object.keys(this.ratios)].map(Number));
    for (const index of indexes) {
      const samePage = old.pages[index]?.pageId === view.pages[index]?.pageId;
      if (samePage && !newFile) continue;
      if (this.images[index] !== undefined) this.#drop(index);
      if (!samePage) delete this.ratios[index];
    }
  }

  /** Page `index` came into view or left it. */
  seen(index: number, visible: boolean) {
    this.seenMany([[index, visible]]);
  }

  /**
   * The pages that came into view or left it at once (one observer callback): all of them are
   * taken before the near pages are worked out, so a jump asks only near where it lands.
   */
  seenMany(changes: [index: number, visible: boolean][]) {
    for (const [index, visible] of changes) {
      if (visible) this.#visible.add(index);
      else this.#visible.delete(index);
    }
    this.#update();
  }

  /**
   * Page `index` is shown alone in the large view (kept within the note); null ends the view.
   * While it is shown, it counts as the page in view: its image and its neighbours are asked
   * for, and the pages far from it are dropped.
   */
  focus(index: number | null) {
    if (this.#closed) return;
    const count = this.view?.pages.length ?? 0;
    this.focused = index === null || count === 0 ? null : Math.min(count - 1, Math.max(0, index));
    this.#update();
  }

  /** The image of page `index` loaded with this size: its placeholder keeps the ratio. */
  measured(index: number, width: number, height: number) {
    if (width > 0 && height > 0) this.ratios[index] = width / height;
  }

  /**
   * The window shows again or gets the focus: hiding it closed the review in Rust, so near
   * pages are asked again. A failed open or reload is tried again.
   */
  refresh() {
    if (this.error !== null || this.stale !== null) void this.reload();
    this.#update();
  }

  /** The review ends: its calls in Rust end, later answers are not shown. */
  close() {
    if (this.#closed) return;
    this.#closed = true;
    this.#asks.clear();
    this.images = {};
    invoke("note_close", { path: this.path }).catch((error: unknown) =>
      console.error("notedown: note_close failed", error),
    );
  }

  /** The pages in view: the page shown alone, else those of the pages side. */
  #shown(): Iterable<number> {
    return this.focused !== null ? [this.focused] : this.#visible;
  }

  #update() {
    if (this.#closed || !this.view) return;
    const pages = nearPages(this.#shown(), this.view.pages.length);
    if (!pages) return;
    const [keepFrom, keepTo] = pages.keep;
    for (const key of Object.keys(this.images)) {
      const index = Number(key);
      if (index >= keepFrom && index <= keepTo) continue;
      this.#drop(index);
    }
    // While the view is not up to date, a new image could be of a file its text does not
    // match: the pages wait for the next reload that works.
    if (this.stale !== null) return;
    for (let index = pages.near[0]; index <= pages.near[1]; index++) {
      if (this.images[index] === undefined) this.#ask(index);
    }
  }

  #near(index: number): boolean {
    const pages = this.view ? nearPages(this.#shown(), this.view.pages.length) : null;
    return pages !== null && index >= pages.near[0] && index <= pages.near[1];
  }

  #ask(index: number) {
    const ask = (this.#asks.get(index) ?? 0) + 1;
    this.#asks.set(index, ask);
    this.images[index] = { state: "loading" };
    const current = () => !this.#closed && this.#asks.get(index) === ask;
    invoke<string>("note_page_image", { path: this.path, index })
      .then((url) => {
        if (current()) this.images[index] = { state: "shown", url };
      })
      .catch((error: unknown) => {
        if (!current()) return;
        const why = text(error);
        if (ENDED.has(why)) {
          delete this.images[index];
          this.#asks.set(index, ask + 1);
          // A drop of an older ask ended this one in Rust too: a page still near is asked again.
          if (why === DROPPED && this.#near(index)) this.#update();
        } else {
          this.images[index] = { state: "failed", text: why };
        }
      });
  }

  /** Page `index` went far away: its URL is released, a call that still waits is ended in Rust. */
  #drop(index: number) {
    const image = this.images[index];
    this.#asks.set(index, (this.#asks.get(index) ?? 0) + 1);
    delete this.images[index];
    if (image?.state === "loading") {
      invoke("note_page_drop", { path: this.path, index }).catch((error: unknown) =>
        console.error("notedown: note_page_drop failed", error),
      );
    }
  }
}
