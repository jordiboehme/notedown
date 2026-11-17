import type { Entry, FolderState, LibraryState, LibraryStateView, ShownListing } from "../lib/library-types";
import { formatBytes } from "../settings/format";

// The view models of the library window: pure functions of what the store holds. The
// components only draw what these give.

/** The six device trees, in the order of the sidebar, with the name the window shows. */
export const ROOTS: readonly { path: string; name: string }[] = [
  { path: "NOTE/Note", name: "Note" },
  { path: "DOCUMENT/Document", name: "Document" },
  { path: "INBOX", name: "Inbox" },
  { path: "EXPORT", name: "Export" },
  { path: "SCREENSHOT", name: "Screenshot" },
  { path: "NOTE/MyStyle", name: "MyStyle" },
];

/** What the main area shows. */
export type Selection = { kind: "new" } | { kind: "all" } | { kind: "bin" } | { kind: "folder"; path: string };

/** The key of a listing in the store: the tree's listing of a path and the list's are apart. */
export function listingKey(path: string, foldersOnly: boolean): string {
  return `${foldersOnly ? "tree" : "list"}:${path}`;
}

/** Natural, case-insensitive name order: "file 9" before "File 10". */
const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });

function byName(a: Entry, b: Entry): number {
  return collator.compare(a.name, b.name) || a.id - b.id;
}

function count(n: number | null): string | null {
  return n === null || n === 0 ? null : n.toLocaleString("en-US");
}

export type SidebarItem = { id: "new" | "all" | "bin"; label: string; count: string | null; selected: boolean };

/**
 * The fixed items of the sidebar: New with its count and All notes (the notes are on this Mac,
 * so they show in every state), the device trees and the recycle bin only while the library
 * can be read. A count is not shown before the notes are read, nor for no new notes.
 */
export function sidebarItems(
  view: LibraryStateView | null,
  counts: { newNotes: number | null; allNotes: number | null },
  selection: Selection,
): { notes: SidebarItem[]; tree: boolean; bin: SidebarItem | null } {
  const ready = view?.state === "ready";
  return {
    notes: [
      { id: "new", label: "New", count: count(counts.newNotes), selected: selection.kind === "new" },
      { id: "all", label: "All notes", count: count(counts.allNotes), selected: selection.kind === "all" },
    ],
    tree: ready,
    bin: ready ? { id: "bin", label: "Recycle bin", count: null, selected: selection.kind === "bin" } : null,
  };
}

export type TreeRow = {
  path: string;
  name: string;
  depth: number;
  open: boolean;
  /** Open and not fully listed yet. */
  loading: boolean;
  /** Listed and without folders: no disclosure triangle. */
  empty: boolean;
  error: string | null;
  selected: boolean;
};

/** Folders before files; names in natural order. Server order while the folder loads. */
function ordered(folder: FolderState): Entry[] {
  if (!folder.complete) return folder.entries;
  return [...folder.entries].sort(byName);
}

/**
 * The tree in the sidebar: the device trees and, below each open folder, its folders. A
 * folder is listed only when it is opened (`missing`: open, with no listing yet); an open
 * folder with more pages goes on loading (`more`). The tree takes only its own folders-only
 * listings: the list's listing of a path pages and is followed on its own.
 */
export function treeView(
  listings: Record<string, FolderState>,
  open: ReadonlySet<string>,
  selected: string | null,
): { rows: TreeRow[]; missing: string[]; more: string[] } {
  const rows: TreeRow[] = [];
  const missing: string[] = [];
  const more: string[] = [];
  const visit = (path: string, name: string, depth: number) => {
    const isOpen = open.has(path);
    const listing = listings[listingKey(path, true)];
    const folders = listing ? ordered(listing).filter((e) => e.kind === "folder") : [];
    const row: TreeRow = {
      path,
      name,
      depth,
      open: isOpen,
      loading: false,
      empty: listing !== undefined && listing.complete && folders.length === 0,
      error: listing?.error ?? null,
      selected: path === selected,
    };
    rows.push(row);
    if (!isOpen) return;
    if (!listing) {
      missing.push(path);
      row.loading = true;
      return;
    }
    if (!listing.complete && listing.error === null) {
      more.push(path);
      row.loading = true;
    }
    for (const child of folders) visit(child.path, child.name, depth + 1);
  };
  for (const root of ROOTS) visit(root.path, root.name, 0);
  return { rows, missing, more };
}

export type Sort = { by: "name" | "size" | "date"; ascending: boolean };

/**
 * The rows of the list. While the folder loads they come in the server's order whatever the
 * sort (rows would jump as pages come); once it is complete, folders first, then by the sort:
 * names in natural, case-insensitive order, or size or date with the name as the tie-break.
 */
export function listRows(folder: FolderState, sort: Sort): { rows: Entry[]; sortable: boolean } {
  if (!folder.complete) return { rows: folder.entries, sortable: false };
  const sign = sort.ascending ? 1 : -1;
  const compare = (a: Entry, b: Entry): number => {
    if (sort.by === "size") return sign * ((a.size ?? -1) - (b.size ?? -1)) || byName(a, b);
    if (sort.by === "date") return sign * (a.mtimeMs - b.mtimeMs) || byName(a, b);
    return sign * byName(a, b);
  };
  const rows = [...folder.entries].sort((a, b) => {
    if (a.kind !== b.kind) return a.kind === "folder" ? -1 : 1;
    return compare(a, b);
  });
  return { rows, sortable: true };
}

/** The rows a virtual list puts in the DOM, and the heights of the spacers around them. */
export type Range = { start: number; end: number; top: number; bottom: number };

/**
 * The rows that are visible at `scrollTop` in a view `height` high, with `overscan` rows
 * before and after; the rest are only spacers, so the scroll bar keeps the full height.
 */
export function visibleRange(args: {
  scrollTop: number;
  height: number;
  rowHeight: number;
  count: number;
  overscan?: number;
}): Range {
  const { scrollTop, height, rowHeight, count } = args;
  const overscan = args.overscan ?? 8;
  if (count === 0 || rowHeight <= 0) return { start: 0, end: 0, top: 0, bottom: 0 };
  const first = Math.floor(Math.max(0, scrollTop) / rowHeight);
  const last = Math.ceil((Math.max(0, scrollTop) + Math.max(0, height)) / rowHeight);
  const start = Math.min(count, Math.max(0, first - overscan));
  const end = Math.min(count, last + overscan);
  return { start, end, top: start * rowHeight, bottom: (count - end) * rowHeight };
}

/** How many rows before the end the next page is asked for. */
export const PAGE_AHEAD = 50;

/**
 * Whether the list asks for the folder's next page: there is one, no call for it runs, and the
 * shown rows reach within {@link PAGE_AHEAD} rows of the end.
 */
export function needsNextPage(args: { end: number; count: number; next: string | null; busy: boolean }): boolean {
  return args.next !== null && !args.busy && args.end >= args.count - PAGE_AHEAD;
}

export type Crumb = { label: string; path: string };

/** The path bar: the device tree as one part ("Document"), then each folder below it. */
export function pathBar(path: string): Crumb[] {
  const root = ROOTS.find((r) => path === r.path || path.startsWith(`${r.path}/`));
  const crumbs: Crumb[] = [];
  let at = "";
  let rest = path;
  if (root) {
    crumbs.push({ label: root.name, path: root.path });
    at = root.path;
    rest = path.slice(root.path.length + 1);
  }
  for (const part of rest.split("/").filter((p) => p !== "")) {
    at = at === "" ? part : `${at}/${part}`;
    crumbs.push({ label: part, path: at });
  }
  return crumbs;
}

/**
 * The listings the window shows, for `library_shown`: the open folders of the tree (folders
 * only) and the selected folder (everything), in a fixed order.
 */
export function shownListings(selection: Selection, open: ReadonlySet<string>): ShownListing[] {
  const shown: ShownListing[] = [...open].map((path) => ({ path, foldersOnly: true }));
  if (selection.kind === "folder") shown.push({ path: selection.path, foldersOnly: false });
  return shown.sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : Number(b.foldersOnly) - Number(a.foldersOnly)));
}

/** What each state says when Rust sends no text of its own. */
const STATE_TEXTS: Record<Exclude<LibraryState, "ready">, string> = {
  notSetUp: "Choose where the server runs in Settings.",
  loggedOut: "Log in to see your library.",
  unreachable: "The server does not answer.",
  notImported: "The Supernote has not been imported yet.",
  refused: "The server refused the login. Log in again.",
};

/**
 * What the window says instead of the library while it cannot be read: the text of the state
 * and a Settings button that opens `page`. Nothing when ready; a waiting line before the first
 * binding.
 */
export function stateView(
  view: LibraryStateView | null,
): { text: string; page: string | null; action: "Settings" | null } | null {
  if (view === null) return { text: "Looking at the library...", page: null, action: null };
  if (view.state === "ready") return null;
  return { text: view.text ?? STATE_TEXTS[view.state], page: view.page, action: "Settings" };
}

/** The folder of a file, as the path bar names it: "Note / Work". */
export function folderLabel(path: string): string {
  const cut = path.lastIndexOf("/");
  return cut < 0 ? "" : pathBar(path.slice(0, cut)).map((c) => c.label).join(" / ");
}

/** The size column: nothing for a folder. */
export function sizeText(entry: Entry): string {
  return entry.kind === "folder" || entry.size === null ? "" : formatBytes(entry.size);
}

/** A date and time in the user's format. */
export function dateText(ms: number): string {
  return new Date(ms).toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
}
