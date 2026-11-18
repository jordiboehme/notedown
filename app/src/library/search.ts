// The search of the library window, the pure parts: which notes a name filter keeps, when the
// text is searched and how the result rows are ordered. The state is in search.svelte.ts.

import type { ContentHit, Entry, NoteSummary } from "../lib/library-types";
import type { Selection } from "./view";

/** "This folder" (the shown folder and below it, or the shown list of notes) or "Everywhere". */
export type Scope = "here" | "everywhere";

export const DEFAULT_SCOPE: Scope = "everywhere";

export const SCOPE_LABELS: Record<Scope, string> = { here: "This folder", everywhere: "Everywhere" };

/** How long the search waits after the last keystroke before it asks Rust. */
export const DEBOUNCE_MS = 200;

/** What the results say when Rust found more names than it answers. */
export const MORE_NAMES = "More than 200 names. Type more to narrow the list.";

export const NO_NAMES = "No names match.";
export const NO_TEXT = "No text matches.";

/** A note or a file whose name holds the text. Folders are no results (spec 3). */
export type NameRow = { kind: "note" | "file"; path: string; name: string };

/** One row of the results: a name hit, or a text hit with its block. */
export type ResultRow = NameRow & {
  section: "names" | "text";
  context?: string;
  page?: number;
  pageId?: string;
  blockId?: string;
};

function nameOf(path: string): string {
  return path.slice(path.lastIndexOf("/") + 1);
}

/** True when `name` holds `text`, without case, as Rust's `name_matches`. Empty matches nothing. */
export function nameMatches(name: string, text: string): boolean {
  return text !== "" && name.toLowerCase().includes(text.toLowerCase());
}

/** The notes whose file name holds `text`, without case, in their order. */
export function filterNotes(notes: NoteSummary[], text: string): NoteSummary[] {
  const wanted = text.trim();
  return notes.filter((note) => nameMatches(nameOf(note.path), wanted));
}

/** True when the text is searched too: 2 characters or more without the spaces around them. */
export function wantsContent(text: string): boolean {
  return [...text.trim()].length >= 2;
}

/** True when `text` has a letter or a digit: the text search finds words only. */
export function hasWord(text: string): boolean {
  return /[\p{L}\p{N}]/u.test(text);
}

/** A note is a `.note` file. */
export function noteRow(path: string): NameRow {
  return { kind: /\.note$/i.test(path) ? "note" : "file", path, name: nameOf(path) };
}

/** A file `library_find` answered (it answers files only). */
export function entryRow(entry: Entry): NameRow {
  return noteRow(entry.path);
}

/** The name hits first, then the text hits. A note in both lists is in both. */
export function resultRows(names: NameRow[], content: ContentHit[]): ResultRow[] {
  return [
    ...names.map((row): ResultRow => ({ ...row, section: "names" })),
    ...content.map(
      (hit): ResultRow => ({
        kind: "note",
        path: hit.path,
        name: hit.title,
        section: "text",
        context: hit.context,
        page: hit.page,
        pageId: hit.pageId,
        blockId: hit.blockId,
      }),
    ),
  ];
}

/** What the window shows, as far as the search cares: "This folder" means it. */
export type SearchPlace =
  /** `error` is set when the list failed to load, so a search does not wait for it. */
  | { kind: "new" | "all"; notes: NoteSummary[] | null; error?: string }
  | { kind: "folder"; path: string }
  | { kind: "other" };

/** The place of the search in the shown `selection`, with the lists of New and All notes. */
export function searchPlace(
  selection: Selection,
  newNotes: NoteSummary[] | null,
  allNotes: NoteSummary[] | null,
  errors: { newNotes?: string | null; allNotes?: string | null } = {},
): SearchPlace {
  switch (selection.kind) {
    case "new":
      return listPlace("new", newNotes, errors.newNotes);
    case "all":
      return listPlace("all", allNotes, errors.allNotes);
    case "folder":
      return { kind: "folder", path: selection.path };
    default:
      return { kind: "other" };
  }
}

function listPlace(kind: "new" | "all", notes: NoteSummary[] | null, error: string | null | undefined): SearchPlace {
  return notes === null && error ? { kind, notes, error } : { kind, notes };
}

/** Escape empties the search field only while it holds text; else the window gets it. */
export function fieldTakesEscape(text: string): boolean {
  return text !== "";
}

/**
 * The scope a search uses in `place`: "This folder" means the shown folder or list of notes,
 * so where none is shown (the review, the recycle bin) it searches everywhere.
 */
export function effectiveScope(scope: Scope, place: SearchPlace): Scope {
  return scope === "here" && place.kind === "other" ? "everywhere" : scope;
}

/** Cmd+F focuses the search field. */
export function isFindKey(event: { key: string; metaKey: boolean }): boolean {
  return event.metaKey && event.key.toLowerCase() === "f";
}
