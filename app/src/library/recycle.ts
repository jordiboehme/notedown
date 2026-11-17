import { relativeTime } from "../lib/format";
import type { RecycleItem } from "../lib/library-types";
import { formatBytes } from "../settings/format";
import { folderLabel, needsNextPage, parentOf } from "./view";

// The view models of the recycle bin view: pure functions of what the store holds.

/** The bin as the store holds it: the items loaded so far and the cursor of the next page. */
export type Bin = {
  items: RecycleItem[];
  /** The cursor of the next page; null once every page is loaded. */
  next: string | null;
  /** A first page came (or failed). */
  loaded: boolean;
  /** A `library_recycle` call runs. */
  busy: boolean;
  /** Why the bin could not be read; null when it could. */
  error: string | null;
};

export type RecycleRow = {
  key: string;
  item: RecycleItem;
  name: string;
  kind: RecycleItem["kind"];
  /** The folder it was in, as the path bar names it: "Note / Work". */
  folder: string;
  /** When it was deleted, relative to now. */
  deleted: string;
  /** The size of a file; "4 files" for a folder (nothing when the server does not say). */
  size: string;
};

function counted(n: number, one: string, many: string): string {
  return `${n.toLocaleString("en-US")} ${n === 1 ? one : many}`;
}

/** `items` and then the items of `page` that are not in them yet. */
export function mergeBin(items: RecycleItem[], page: RecycleItem[]): RecycleItem[] {
  const seen = new Set(items.map((i) => i.key));
  const merged = [...items];
  for (const item of page) {
    if (seen.has(item.key)) continue;
    seen.add(item.key);
    merged.push(item);
  }
  return merged;
}

/** The rows of the bin, newest first as the server sends them, each key once. */
export function recycleRows(items: RecycleItem[], now: number): RecycleRow[] {
  return mergeBin([], items).map((item) => ({
    key: item.key,
    item,
    name: item.name,
    kind: item.kind,
    folder: folderLabel(item.path),
    deleted: relativeTime(item.deletedAt, now),
    size:
      item.kind === "file" ? formatBytes(item.size) : item.files === null ? "" : counted(item.files, "file", "files"),
  }));
}

/**
 * What the bin view shows: the first page is loading, the bin is empty (known only once a page
 * came without items and without a next page), and whether Empty can be pressed.
 */
export function binView(bin: Bin): { loading: boolean; empty: boolean; canEmpty: boolean } {
  const known = bin.loaded && bin.error === null;
  const empty = known && bin.items.length === 0 && bin.next === null;
  return { loading: !bin.loaded, empty, canEmpty: known && !empty };
}

/** The question before Empty: with the count once every page is loaded. */
export function emptyQuestion(bin: Pick<Bin, "items" | "next">): string {
  const what = bin.next === null ? counted(bin.items.length, "item", "items") : "all items";
  return `Delete ${what} for good? This cannot be undone.`;
}

/** The question before Empty while it is asked and the bin has something to empty; else null. */
export function emptyConfirm(bin: Bin, asked: boolean): string | null {
  return asked && binView(bin).canEmpty ? emptyQuestion(bin) : null;
}

/** The answer to a restore that asks for a name with a slash: the item stays in its folder. */
export const OWN_FOLDER = "Restore keeps the item in its folder. Type a name without /.";

/**
 * Where a restore goes with the typed name: the folder the item was in. Refused, with the text
 * the row shows, for an empty name, the name that is taken (`taken`), or a name with a slash
 * (the server would make the folders on the way).
 */
export function restoreTarget(path: string, typed: string, taken: string): { path: string } | { refuse: string } {
  const name = typed.trim();
  if (name === "" || name === taken) return { refuse: "Type another name." };
  if (name.includes("/")) return { refuse: OWN_FOLDER };
  const folder = parentOf(path);
  return { path: folder === "" ? name : `${folder}/${name}` };
}

/**
 * Whether the bin asks for its next page: as a folder list does, and not after a page that
 * failed (a first page without rows would ask again at once, without a wait).
 */
export function binNeedsNext(bin: Bin, end: number, count: number): boolean {
  return bin.error === null && needsNextPage({ end, count, next: bin.next, busy: bin.busy });
}
