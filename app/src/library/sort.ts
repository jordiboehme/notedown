import type { NoteSummary } from "../lib/library-types";
import type { RecycleRow } from "./recycle";
import { folderLabel, type Sort } from "./view";

// The sort of the library's lists: the notes lists (New and All notes), a folder's list and the
// recycle bin. A click on a column head sorts by it ascending, a second click descending. The
// order is kept per list kind for this viewer.

/** The kinds of list; New and All notes are both notes lists. */
export type ListKind = "notes" | "folder" | "bin";

export type NotesSort = { by: "name" | "folder" | "pages" | "date"; ascending: boolean };
export type BinSort = { by: "name" | "folder" | "deleted" | "size"; ascending: boolean };
export type FolderSort = Sort;

type Sorts = { notes: NotesSort; folder: FolderSort; bin: BinSort };

/** The columns of each list kind. */
const COLUMNS: { [K in ListKind]: readonly Sorts[K]["by"][] } = {
  notes: ["name", "folder", "pages", "date"],
  folder: ["name", "size", "date"],
  bin: ["name", "folder", "deleted", "size"],
};

/** The order each list came in before it could be sorted. */
export const DEFAULT_SORTS: Sorts = {
  notes: { by: "date", ascending: false },
  folder: { by: "name", ascending: true },
  bin: { by: "deleted", ascending: false },
};

/** Where the sort of each list kind is kept, per viewer. */
export const SORT_KEY: Record<ListKind, string> = {
  notes: "notedown.sort.notes",
  folder: "notedown.sort.folder",
  bin: "notedown.sort.bin",
};

/** The sort after a click on the head of `by`: ascending, or the other way on the same head. */
export function nextSort<S extends { by: string; ascending: boolean }>(sort: S, by: S["by"]): S {
  return { ...sort, by, ascending: sort.by === by ? !sort.ascending : true };
}

/** The sort kept for a list kind; its default without one, or when the storage cannot be read. */
export function readSort<K extends ListKind>(storage: Storage | null, kind: K): Sorts[K] {
  try {
    const saved = JSON.parse(storage?.getItem(SORT_KEY[kind]) ?? "null") as { by?: unknown; ascending?: unknown } | null;
    const columns = COLUMNS[kind] as readonly unknown[];
    if (saved && columns.includes(saved.by) && typeof saved.ascending === "boolean") {
      return { by: saved.by, ascending: saved.ascending } as Sorts[K];
    }
  } catch {
    // Not readable: the default.
  }
  return DEFAULT_SORTS[kind];
}

/** Keeps the sort of a list kind; a storage that fails is no error (it stays for this window). */
export function saveSort<K extends ListKind>(storage: Storage | null, kind: K, sort: Sorts[K]): void {
  try {
    storage?.setItem(SORT_KEY[kind], JSON.stringify({ by: sort.by, ascending: sort.ascending }));
  } catch {
    // Not kept: the next window starts with the default.
  }
}

/** Natural, case-insensitive order, as the Finder sorts names: "Note 2" before "Note 10". */
const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });

/**
 * `items` sorted by `value` in the given direction; equal values by name (ascending), then in
 * the order they came (the sort is stable). The given list stays as it is.
 */
function sorted<T>(items: readonly T[], ascending: boolean, compare: (a: T, b: T) => number, name: (item: T) => string): T[] {
  const sign = ascending ? 1 : -1;
  return [...items].sort((a, b) => sign * compare(a, b) || collator.compare(name(a), name(b)));
}

/** The notes of a notes list in the order of `sort`. */
export function sortNotes(notes: readonly NoteSummary[], sort: NotesSort): NoteSummary[] {
  const compare: Record<NotesSort["by"], (a: NoteSummary, b: NoteSummary) => number> = {
    name: (a, b) => collator.compare(a.title, b.title),
    folder: (a, b) => collator.compare(folderLabel(a.path), folderLabel(b.path)),
    pages: (a, b) => a.pages - b.pages,
    date: (a, b) => a.changedMs - b.changedMs,
  };
  return sorted(notes, sort.ascending, compare[sort.by], (n) => n.title);
}

/** The rows of the bin in the order of `sort`. A folder has no size of its own: it is the smallest. */
export function sortBin(rows: readonly RecycleRow[], sort: BinSort): RecycleRow[] {
  const size = (row: RecycleRow) => (row.kind === "file" ? row.item.size : -1);
  const compare: Record<BinSort["by"], (a: RecycleRow, b: RecycleRow) => number> = {
    name: (a, b) => collator.compare(a.name, b.name),
    folder: (a, b) => collator.compare(a.folder, b.folder),
    deleted: (a, b) => a.item.deletedAt - b.item.deletedAt,
    size: (a, b) => size(a) - size(b),
  };
  return sorted(rows, sort.ascending, compare[sort.by], (r) => r.name);
}

/**
 * Whether a list that comes in pages (a folder, the bin) loads all its pages at once: its sort
 * is not the order the pages come in, or the user clicked a head. Else the next page comes
 * when the rows near the end come into view.
 */
export function loadsRest(kind: "folder" | "bin", sort: FolderSort | BinSort, clicked: boolean): boolean {
  const start = DEFAULT_SORTS[kind];
  return clicked || sort.by !== start.by || sort.ascending !== start.ascending;
}
