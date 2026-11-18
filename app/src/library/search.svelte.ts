// The search of the library window: the typed text, the scope and the result rows. The names of
// a list of notes (New, All notes) are filtered at once; Rust's name search (`library_find`) and
// text search (`notes_search`) run 200 ms after the last keystroke. Every run has a number, and
// an answer of an older run is dropped, so a slow answer never replaces a newer one.

import { invoke } from "@tauri-apps/api/core";
import type { ContentHit, Found, NoteSummary } from "../lib/library-types";
import {
  DEBOUNCE_MS,
  DEFAULT_SCOPE,
  effectiveScope,
  entryRow,
  filterNotes,
  hasWord,
  noteRow,
  resultRows,
  wantsContent,
  type NameRow,
  type ResultRow,
  type Scope,
  type SearchPlace,
} from "./search";

export type { SearchPlace } from "./search";

export class LibrarySearch {
  text = $state("");
  scope = $state<Scope>(DEFAULT_SCOPE);
  names = $state<NameRow[]>([]);
  content = $state<ContentHit[]>([]);
  /** Rust found more names than it answered. */
  more = $state(false);
  /** The text of the last failed call, shown in one line under the field. */
  error = $state<string | null>(null);
  /** A run waits or its calls are not all answered. */
  busy = $state(false);
  rows: ResultRow[] = $derived(resultRows(this.names, this.content));

  #place: () => SearchPlace;
  #seq = 0;
  #timer: ReturnType<typeof setTimeout> | null = null;
  /** What the last run was for ([`#signature`]): the same place runs nothing again. */
  #ranFor = "";

  constructor(place: () => SearchPlace) {
    this.#place = place;
  }

  /** The scope the search uses now: "This folder" where no folder or list is shown is Everywhere. */
  get shownScope(): Scope {
    return effectiveScope(this.scope, this.#place());
  }

  /** "This folder" means something only where a folder or a list of notes is shown. */
  get hereAvailable(): boolean {
    return this.#place().kind !== "other";
  }

  /** The user typed: the names of a shown list follow at once, the rest after the wait. */
  type(text: string) {
    this.text = text;
    this.#schedule();
  }

  setScope(scope: Scope) {
    if (scope === this.scope) return;
    this.scope = scope;
    this.#schedule();
  }

  /**
   * The shown place or its list of notes changed: a search that is on runs again when that
   * changes what it searches (not for Everywhere, not for the same list).
   */
  rerun() {
    if (this.text.trim() !== "" && this.#signature() !== this.#ranFor) this.#schedule();
  }

  /** Empties the field and the rows; a waiting run never starts and pending answers are dropped. */
  clear() {
    this.text = "";
    this.#stop();
    this.#reset();
  }

  #stop() {
    this.#seq += 1;
    if (this.#timer !== null) clearTimeout(this.#timer);
    this.#timer = null;
    this.busy = false;
  }

  #reset() {
    this.names = [];
    this.content = [];
    this.more = false;
    this.error = null;
  }

  /**
   * The shown list of notes when the scope is "This folder" in New or All notes: `notes` is
   * null while the list still loads. Null for any other search.
   */
  #listed(): { notes: NoteSummary[] | null } | null {
    const place = this.#place();
    if (this.scope !== "here" || (place.kind !== "new" && place.kind !== "all")) return null;
    return { notes: place.notes };
  }

  /** The list a run filters by; null for a search that has none. */
  #list(): NoteSummary[] | null {
    return this.#listed()?.notes ?? null;
  }

  /** What a run searches: the scope used, the folder, or the keys of the shown list. */
  #signature(): string {
    const place = this.#place();
    if (this.shownScope === "everywhere") return "everywhere";
    const listed = this.#listed();
    if (listed) return `${place.kind}:${listed.notes === null ? "loading" : listed.notes.map((n) => n.key).join(",")}`;
    return place.kind === "folder" ? `folder:${place.path}` : place.kind;
  }

  #schedule() {
    this.#stop();
    this.#ranFor = this.#signature();
    const text = this.text.trim();
    if (text === "") {
      this.#reset();
      return;
    }
    const listed = this.#listed();
    if (listed && listed.notes === null) {
      // The list still loads: no rows yet and no false empty text; it runs once it comes.
      this.#reset();
      this.busy = true;
      return;
    }
    const list = listed?.notes ?? null;
    if (list) {
      this.names = filterNotes(list, text).map((note) => noteRow(note.path));
      this.more = false;
    }
    this.busy = true;
    const seq = this.#seq;
    this.#timer = setTimeout(() => {
      this.#timer = null;
      void this.#run(seq, text);
    }, DEBOUNCE_MS);
  }

  async #run(seq: number, text: string) {
    const current = () => seq === this.#seq;
    const place = this.#place();
    const list = this.#list();
    const under = this.shownScope === "here" && place.kind === "folder" ? place.path : null;
    this.error = null;
    const fail = (error: unknown) => {
      if (current()) this.error = String(error);
    };
    const calls: Promise<void>[] = [];
    if (!list) {
      calls.push(
        invoke<Found>("library_find", { text, under: under ?? "" }).then(
          (found) => {
            if (!current()) return;
            this.names = found.entries.map(entryRow);
            this.more = found.more;
          },
          (error) => {
            if (!current()) return;
            this.names = [];
            this.more = false;
            fail(error);
          },
        ),
      );
    }
    if (wantsContent(text) && hasWord(text)) {
      const keys = list ? new Set(list.map((note) => note.key)) : null;
      calls.push(
        invoke<ContentHit[]>("notes_search", { query: text, under }).then(
          (hits) => {
            if (current()) this.content = keys ? hits.filter((hit) => keys.has(hit.key)) : hits;
          },
          (error) => {
            if (!current()) return;
            this.content = [];
            fail(error);
          },
        ),
      );
    } else {
      this.content = [];
    }
    await Promise.all(calls);
    if (current()) this.busy = false;
  }
}
