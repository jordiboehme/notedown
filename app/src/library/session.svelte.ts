import { invoke } from "@tauri-apps/api/core";
import type { NoteView } from "../lib/library-types";
import { nearPages } from "./review";

/** The image of a page: asked for, shown (a data URL), or why there is none. */
export type PageImage = { state: "loading" } | { state: "shown"; url: string } | { state: "failed"; text: string };

/** What Rust answers to a page whose call was dropped (`note_page_drop`). */
const DROPPED = "The page is not shown anymore.";
/** What Rust answers to a page call that the window ended itself: no failure, the placeholder stays. */
const ENDED = new Set([DROPPED, "The review was closed."]);

function text(error: unknown): string {
  return typeof error === "string" ? error : error instanceof Error ? error.message : String(error);
}

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
  /** The images by page index; a page without one shows its placeholder. */
  images = $state<Record<number, PageImage>>({});
  /** Width by height of the images loaded so far, by page index; kept after a drop. */
  ratios = $state<Record<number, number>>({});

  #visible = new Set<number>();
  /** The `note_open` that runs, and whether another one was asked for meanwhile. */
  #loading: Promise<void> | null = null;
  #again = false;
  /** Counts the asks of each page: an answer of an older ask (dropped since) is not shown. */
  #asks = new Map<number, number>();
  #closed = false;

  constructor(path: string) {
    this.path = path;
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
      // A reload that fails keeps what is shown.
      if (!this.#closed && !this.view) this.error = text(error);
      return;
    }
    if (this.#closed) return;
    const old = this.view;
    this.view = view;
    this.error = null;
    if (old) this.#keepImages(old, view);
    if (view.key !== null && (old === null || old.key === null || textOf(old) !== textOf(view))) {
      invoke("note_seen", { key: view.key }).catch((error: unknown) =>
        console.error("notedown: note_seen failed", error),
      );
    }
    this.#update();
  }

  /** Drops the images (and ratios) of the indexes whose page is another one now, or gone. */
  #keepImages(old: NoteView, view: NoteView) {
    const indexes = new Set([...Object.keys(this.images), ...Object.keys(this.ratios)].map(Number));
    for (const index of indexes) {
      if (old.pages[index]?.pageId === view.pages[index]?.pageId) continue;
      if (this.images[index] !== undefined) this.#drop(index);
      delete this.ratios[index];
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

  /** The image of page `index` loaded with this size: its placeholder keeps the ratio. */
  measured(index: number, width: number, height: number) {
    if (width > 0 && height > 0) this.ratios[index] = width / height;
  }

  /** The window shows again: hiding it closed the review in Rust, so near pages are asked again. */
  refresh() {
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

  #update() {
    if (this.#closed || !this.view) return;
    const pages = nearPages(this.#visible, this.view.pages.length);
    if (!pages) return;
    const [keepFrom, keepTo] = pages.keep;
    for (const key of Object.keys(this.images)) {
      const index = Number(key);
      if (index >= keepFrom && index <= keepTo) continue;
      this.#drop(index);
    }
    for (let index = pages.near[0]; index <= pages.near[1]; index++) {
      if (this.images[index] === undefined) this.#ask(index);
    }
  }

  #near(index: number): boolean {
    const pages = this.view ? nearPages(this.#visible, this.view.pages.length) : null;
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
