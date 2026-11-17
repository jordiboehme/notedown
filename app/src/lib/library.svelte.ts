import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { listingKey, type LibraryView } from "../library/view";
import {
  LIBRARY_EVENT,
  type FolderState,
  type Job,
  type LibraryEvent,
  type LibraryStateView,
  type Listing,
  type ShownListing,
} from "./library-types";

// The library window's store: what Rust sent about the library. Every `LibraryEvent` goes
// through `applyEvent`; the listings are keyed by `listingKey` (the tree's and the list's of the
// same path stay apart).

/** The answer of a call that crossed a new binding: the `relist` event follows. */
const CHANGED = "The library changed. List again.";

type Store = {
  /** The state of the library; null before the first binding. */
  view: LibraryStateView | null;
  /** Each listing as loaded so far. */
  listings: Record<string, FolderState>;
  /** The cursor of each listing's next page; null once it is complete. */
  next: Record<string, string | null>;
  jobs: Record<number, Job>;
  /** Counts the new bindings: the window lists what it shows again when it moves. */
  generation: number;
  /** Counts the `recycle` events: the bin view lists again when it moves. */
  recycle: number;
  /** The listings with a `library_list` call running (kept in the state so the list sees it). */
  busy: Record<string, true>;
  /** The folders are shown as a tree or as columns; saved as the start view. */
  viewMode: LibraryView;
};

export const library = $state<Store>({
  view: null,
  listings: {},
  next: {},
  jobs: {},
  generation: 0,
  recycle: 0,
  busy: {},
  viewMode: "tree",
});

/** What `library_shown` was last told, to call it only on a change. */
let lastShown: string | null = null;
/**
 * The keys of the listings Rust follows (null: not told yet, take every listing). A listing
 * outside them is not followed, so an event or an answer for it that comes late is not kept:
 * the folder opened again is listed fresh.
 */
let shownKeys: Set<string> | null = null;
/** Whether the user chose a view in this window: a config read that answers later is stale. */
let chosen = false;

function isShown(key: string): boolean {
  return shownKeys === null || shownKeys.has(key);
}

/** Back to the start (for the tests). */
export function resetLibrary() {
  library.view = null;
  library.listings = {};
  library.next = {};
  library.jobs = {};
  library.generation = 0;
  library.recycle = 0;
  library.busy = {};
  library.viewMode = "tree";
  lastShown = null;
  shownKeys = null;
  chosen = false;
  saved = null;
  saving = null;
}

function report(command: string) {
  return (error: unknown) => console.error(`notedown: ${command} failed`, error);
}

/** Applies one event of Rust to the store. */
export function applyEvent(event: LibraryEvent) {
  switch (event.type) {
    case "state":
      library.view = { state: event.state, text: event.text, page: event.page };
      break;
    case "folder": {
      const key = listingKey(event.folder.path, event.foldersOnly);
      if (!isShown(key)) break;
      library.listings[key] = event.folder;
      library.next[key] = event.next;
      break;
    }
    case "job":
      library.jobs[event.job.id] = event.job;
      break;
    case "relist":
      library.listings = {};
      library.next = {};
      library.busy = {};
      library.generation += 1;
      lastShown = null;
      shownKeys = null;
      break;
    case "recycle":
      library.recycle += 1;
      break;
    case "drop":
      // Uploads come with the drop target; until then the dropped paths are let go.
      invoke("library_cancel_drop", { dropId: event.dropId }).catch(report("library_cancel_drop"));
      break;
  }
}

/**
 * Follows the library's events and asks once for its state as it is now. An event that comes
 * before that answer is newer, so the answer is then dropped. Returns the function that stops
 * following.
 */
export async function startLibrary(): Promise<() => void> {
  let heard = false;
  const unlisten = await listen<LibraryEvent>(LIBRARY_EVENT, (event) => {
    if (event.payload.type === "state") heard = true;
    applyEvent(event.payload);
  });
  invoke<LibraryStateView | null>("library_state")
    .then((first) => {
      if (!heard && first) library.view = first;
    })
    .catch(report("library_state"));
  return unlisten;
}

/** Whether a `library_list` call of this listing runs. */
export function isBusy(path: string, foldersOnly: boolean): boolean {
  return library.busy[listingKey(path, foldersOnly)] === true;
}

/** Calls `library_list` once per listing at a time, and drops an answer of an older binding. */
async function list(path: string, foldersOnly: boolean, cursor: string | null) {
  const key = listingKey(path, foldersOnly);
  if (library.busy[key]) return;
  const generation = library.generation;
  library.busy[key] = true;
  try {
    const listing = await invoke<Listing>("library_list", { path, cursor, foldersOnly });
    if (library.generation !== generation || !isShown(key)) return;
    library.listings[key] = listing.folder;
    library.next[key] = listing.next;
  } catch (error) {
    const text = String(error);
    if (library.generation !== generation || text === CHANGED || !isShown(key)) return;
    const before = library.listings[key];
    library.listings[key] = {
      path,
      entries: before?.entries ?? [],
      complete: before?.complete ?? false,
      loading: false,
      error: text,
    };
  } finally {
    if (library.generation === generation) delete library.busy[key];
  }
}

/** Lists a folder from its first page (the list) or only its folders (the tree). */
export function listFolder(path: string, foldersOnly: boolean): Promise<void> {
  return list(path, foldersOnly, null);
}

/** Lists the next page of a listing, when it has one. */
export function listNext(path: string, foldersOnly: boolean): Promise<void> {
  const cursor = library.next[listingKey(path, foldersOnly)];
  if (!cursor) return Promise.resolve();
  return list(path, foldersOnly, cursor);
}

/**
 * Tells Rust which listings the window shows, when they changed since the last time. Rust
 * follows only these, so the store forgets the others: a folder opened again is listed again.
 */
export function showListings(listings: ShownListing[]) {
  const key = JSON.stringify(listings);
  if (key === lastShown) return;
  lastShown = key;
  const kept = new Set(listings.map((l) => listingKey(l.path, l.foldersOnly)));
  shownKeys = kept;
  for (const old of Object.keys(library.listings)) if (!kept.has(old)) delete library.listings[old];
  for (const old of Object.keys(library.next)) if (!kept.has(old)) delete library.next[old];
  invoke("library_shown", { listings }).catch(report("library_shown"));
}

/** Reads the saved start view; the window starts in it. Stays in the tree when it cannot be read. */
export async function loadView(): Promise<void> {
  try {
    const config = await invoke<{ startView: LibraryView }>("get_config");
    if (!chosen && (config.startView === "tree" || config.startView === "columns")) library.viewMode = config.startView;
  } catch (error) {
    report("get_config")(error);
  }
}

/** The view Rust has (or is being told); null before any save. */
let saved: LibraryView | null = null;
let saving: Promise<void> | null = null;

/** One save at a time: after it, the latest choice is saved when it differs from what was saved. */
async function saveView(): Promise<void> {
  if (saving) return saving;
  saving = (async () => {
    try {
      while (saved !== library.viewMode) {
        const view = library.viewMode;
        saved = view;
        try {
          await invoke("set_start_view", { startView: view });
        } catch (error) {
          report("set_start_view")(error);
          saved = null;
          return;
        }
      }
    } finally {
      saving = null;
    }
  })();
  return saving;
}

/** Shows the folders in this view at once and saves the choice as the start view. */
export async function chooseView(view: LibraryView): Promise<void> {
  chosen = true;
  if (library.viewMode === view && (saving === null || saved === view)) return;
  library.viewMode = view;
  await saveView();
}
