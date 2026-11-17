import { invoke } from "@tauri-apps/api/core";
import type { NoteView } from "../lib/library-types";
import { nearPages } from "./review";

/** The image of a page: asked for, shown (a data URL), or why there is none. */
export type PageImage = { state: "loading" } | { state: "shown"; url: string } | { state: "failed"; text: string };

/** What Rust answers to a page call that the window ended itself: no failure, the placeholder stays. */
const ENDED = new Set(["The page is not shown anymore.", "The review was closed."]);

function text(error: unknown): string {
  return typeof error === "string" ? error : error instanceof Error ? error.message : String(error);
}

/**
 * One review of the note at `path`: its text from `note_open`, and the images of the pages
 * near the view. The window tells it which pages are in view (`seen`); it asks Rust for the
 * near ones and drops the far ones. `close` ends it in Rust (`note_close`).
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
  /** Counts the asks of each page: an answer of an older ask (dropped since) is not shown. */
  #asks = new Map<number, number>();
  #closed = false;

  constructor(path: string) {
    this.path = path;
  }

  /** Reads the note; opening it marks its text seen. */
  async open(): Promise<void> {
    try {
      const view = await invoke<NoteView>("note_open", { path: this.path });
      if (this.#closed) return;
      this.view = view;
      this.error = null;
      invoke("note_seen", { key: view.key }).catch((error: unknown) =>
        console.error("notedown: note_seen failed", error),
      );
      this.#update();
    } catch (error) {
      if (!this.#closed) this.error = text(error);
    }
  }

  /** Page `index` came into view or left it. */
  seen(index: number, visible: boolean) {
    if (visible) this.#visible.add(index);
    else this.#visible.delete(index);
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
