import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  answerAsk,
  clearedJobs,
  dropPlan,
  entryAt,
  newName,
  type AskChoice,
  type Change,
  type JobAction,
  type Point,
  type UploadArgs,
} from "../library/actions";
import { listingKey, type LibraryView } from "../library/view";
import { mergeBin, restorePath, type Bin } from "../library/recycle";
import {
  LIBRARY_EVENT,
  type FolderState,
  type Job,
  type LibraryEvent,
  type LibraryStateView,
  type Entry,
  type Listing,
  type RecycleItem,
  type RecyclePage,
  type ShownListing,
  type UploadAsk,
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
  /** When each job ended (done or cancelled), in ms: its row goes after a while. */
  ended: Record<number, number>;
  /** The questions of uploads that wait for an answer, the oldest first. */
  asks: Ask[];
  /** A short text about the last action that did not work (a drop, a download); null when none. */
  notice: string | null;
  /** How many jobs each drop had, while any of them is kept: its title does not shrink. */
  dropTotals: Record<number, number>;
  /** The recycle bin as listed so far. */
  bin: Bin;
  /** The restores that run or ask for another name, by the key of their item. */
  restores: Record<string, Restore>;
};

/**
 * A restore of an item of the bin: it runs (`busy`), or the place is taken and it asks for
 * another name (`asking`, with the job's text and the name that is taken). `path` is where the
 * item was.
 */
export type Restore = { job: number | null; state: "busy" | "asking"; text: string | null; name: string; path: string };

function noBin(): Bin {
  return { items: [], next: null, loaded: false, busy: false, error: null };
}

/** An upload that asks before it starts: the next call of `library_upload` and its question. */
export type Ask = UploadArgs & { ask: UploadAsk };

export const library = $state<Store>({
  view: null,
  listings: {},
  next: {},
  jobs: {},
  generation: 0,
  recycle: 0,
  busy: {},
  viewMode: "tree",
  ended: {},
  asks: [],
  notice: null,
  dropTotals: {},
  bin: noBin(),
  restores: {},
});

/**
 * Finds the folder under a drop (physical pixels of the web view); null when there is none.
 * The window sets it: it needs the DOM.
 */
let dropTarget: ((position: Point) => string | null) | null = null;

/** The jobs counted for each drop (by drop id), for `dropTotals`. */
const counted = new Map<number, Set<number>>();

/** What `library_shown` was last told, to call it only on a change. */
let lastShown: string | null = null;
/**
 * The keys of the listings Rust follows (null: not told yet, take every listing). A listing
 * outside them is not followed, so an event or an answer for it that comes late is not kept:
 * the folder opened again is listed fresh.
 */
let shownKeys: Set<string> | null = null;
/** Counts the bin's page 1 calls: an answer of an older one is dropped. */
let binEpoch = 0;
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
  library.ended = {};
  library.asks = [];
  library.notice = null;
  library.dropTotals = {};
  library.bin = noBin();
  library.restores = {};
  binEpoch += 1;
  counted.clear();
  dropTarget = null;
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
      keepJob(event.job);
      break;
    case "relist":
      library.listings = {};
      library.next = {};
      library.busy = {};
      library.generation += 1;
      // Rust forgot the drops of the old binding: their questions cannot be answered.
      library.asks = [];
      // The bin is listed again; Rust ends the restores that ask (their job events follow).
      library.bin = noBin();
      binEpoch += 1;
      lastShown = null;
      shownKeys = null;
      break;
    case "recycle":
      library.recycle += 1;
      break;
    case "drop":
      // The folder is found at once: what is under the pointer can change.
      void startDrop(event.dropId, dropTarget ? dropTarget(event.position) : null, dropTarget !== null);
      break;
  }
}

/** Keeps a job as Rust sent or answered it, and notes when it ended. */
function keepJob(job: Job) {
  const ended = job.state === "done" || job.state === "cancelled";
  // A done or cancelled job never runs again: a running event that comes after the answer
  // of its command is older.
  if (!ended && library.ended[job.id] !== undefined) return;
  library.jobs[job.id] = job;
  if (ended && library.ended[job.id] === undefined) library.ended[job.id] = Date.now();
  if (job.kind === "change") followRestore(job);
  if (job.dropId !== null) countJob(job.dropId, job.id);
}

function countJob(dropId: number, id: number) {
  let ids = counted.get(dropId);
  if (!ids) {
    ids = new Set();
    counted.set(dropId, ids);
  }
  if (ids.has(id)) return;
  ids.add(id);
  library.dropTotals[dropId] = ids.size;
}

/** Forgets the jobs `ids`, and the total of a drop none of whose jobs is kept anymore. */
function forgetJobs(ids: number[]) {
  if (ids.length === 0) return;
  for (const id of ids) {
    delete library.jobs[id];
    delete library.ended[id];
  }
  const kept = new Set(Object.values(library.jobs).map((job) => job.dropId));
  for (const dropId of [...counted.keys()]) {
    if (kept.has(dropId)) continue;
    counted.delete(dropId);
    delete library.dropTotals[dropId];
  }
}

/** Forgets the jobs whose rows are cleared at `now`. */
export function clearJobs(now: number) {
  forgetJobs(clearedJobs(library.jobs, library.ended, now));
}

/** Sets how the window finds the folder of a drop (null: the window is gone). */
export function setDropTarget(find: ((position: Point) => string | null) | null) {
  dropTarget = find;
}

/** Rust forgets the paths of a drop that is not uploaded. */
function letGo(dropId: number) {
  return invoke("library_cancel_drop", { dropId }).catch(report("library_cancel_drop"));
}

async function startDrop(dropId: number, folder: string | null, located: boolean) {
  if (!located) {
    await letGo(dropId);
    return;
  }
  const plan = dropPlan(folder);
  if ("refuse" in plan) {
    library.notice = plan.refuse;
    await letGo(dropId);
    return;
  }
  await upload({ dropId, folder: plan.upload });
}

/** Calls `library_upload`; a question waits in `asks`, an error lets the drop go. */
async function upload(args: UploadArgs) {
  try {
    const ask = await invoke<UploadAsk | null>("library_upload", args);
    if (ask) library.asks.push({ ...args, ask });
  } catch (error) {
    library.notice = String(error);
    await letGo(args.dropId);
  }
}

/** The answer to the question of the drop `dropId`: the upload goes on, or the drop is let go. */
export async function answerUpload(dropId: number, choice: AskChoice) {
  const at = library.asks.findIndex((a) => a.dropId === dropId);
  if (at < 0) return;
  const { ask, ...args } = library.asks[at];
  library.asks.splice(at, 1);
  const next = answerAsk(args, ask, choice);
  if (next === null) await letGo(dropId);
  else await upload(next);
}

/** Runs a command; its error becomes the notice. */
async function run<T>(command: string, args: Record<string, unknown>): Promise<T | undefined> {
  try {
    return await invoke<T>(command, args);
  } catch (error) {
    library.notice = String(error);
    return undefined;
  }
}

/** A button of a job row, or Cancel all of its drop. */
export async function jobAction(job: Job, action: JobAction | "cancelAll") {
  switch (action) {
    case "replace":
    case "keepBoth":
    case "skip":
      await run("library_answer", { job: job.id, choice: action });
      break;
    case "retry":
      await run("library_retry", { job: job.id });
      break;
    case "cancel":
      await run("library_cancel", { job: job.id });
      break;
    case "cancelAll":
      if (job.dropId !== null) await run("library_cancel_drop", { dropId: job.dropId });
      break;
    case "reveal":
      if (job.result) await run("library_reveal", { path: job.result });
      break;
    case "clear":
      forgetJobs([job.id]);
      break;
  }
}

/** Retry all: tries the failed uploads of a drop again, one call each. */
export async function retryJobs(ids: number[]) {
  for (const id of ids) await run("library_retry", { job: id });
}

/** Runs a change or a download and keeps the job it answers (also answered). */
async function change(command: string, args: Record<string, unknown>): Promise<Job | undefined> {
  const job = await run<Job>(command, args);
  if (job) keepJob(job);
  return job;
}

export function moveEntry(id: number, toFolder: string) {
  return change("library_move", { id, toFolder });
}

export function renameEntry(id: number, name: string) {
  return change("library_rename", { id, name });
}

export function newFolder(parent: string, name: string) {
  return change("library_new_folder", { parent, name });
}

export function deleteEntry(id: number) {
  return change("library_delete", { id });
}

export function downloadEntry(id: number) {
  return change("library_download", { id });
}

/** The change a done job made to `from`: Rust answers the new path in `result`. */
function changed(job: Job | undefined, from: string, deleted = false): Change | null {
  if (job?.state !== "done") return null;
  if (deleted) return { from, to: null };
  return job.result ? { from, to: job.result } : null;
}

/** Moves the entry `id` at `from` into `toFolder`; the change once it is done. */
export async function moveRow(id: number, from: string, toFolder: string): Promise<Change | null> {
  return changed(await moveEntry(id, toFolder), from);
}

/** Renames an entry to the typed name (null: given up); the change once it is done. */
export async function renameRow(entry: Entry, typed: string | null): Promise<Change | null> {
  const name = typed === null ? null : newName(typed, entry.name);
  if (name === null) return null;
  return changed(await renameEntry(entry.id, name), entry.path);
}

/** Makes a folder with the typed name (null: given up) in `parent`. */
export async function makeFolder(parent: string | null, typed: string | null): Promise<void> {
  const name = typed === null ? null : newName(typed, "");
  if (parent === null || name === null) return;
  await newFolder(parent, name);
}

/** Deletes an entry; the change once it is done. */
export async function deleteRow(entry: Entry): Promise<Change | null> {
  return changed(await deleteEntry(entry.id), entry.path, true);
}

/** The entry at `path` in any listing; null for none or a device tree. */
export function entryOf(path: string | null): Entry | null {
  return path === null ? null : entryAt(path, library.listings);
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
export function showListings(listings: ShownListing[], binShown = false) {
  const key = JSON.stringify([listings, binShown]);
  if (key === lastShown) return;
  lastShown = key;
  const kept = new Set(listings.map((l) => listingKey(l.path, l.foldersOnly)));
  shownKeys = kept;
  for (const old of Object.keys(library.listings)) if (!kept.has(old)) delete library.listings[old];
  for (const old of Object.keys(library.next)) if (!kept.has(old)) delete library.next[old];
  invoke("library_shown", { listings, binShown }).catch(report("library_shown"));
}

/** Lists the bin from page 1; an answer of an older call (page 1 or a next page) is dropped. */
export async function listBin(): Promise<void> {
  const epoch = ++binEpoch;
  library.bin.busy = true;
  try {
    const answer = await invoke<RecyclePage>("library_recycle", { cursor: null });
    if (epoch !== binEpoch) return;
    library.bin = { items: mergeBin([], answer.items), next: answer.next, loaded: true, busy: false, error: null };
  } catch (error) {
    if (epoch !== binEpoch) return;
    library.bin = { ...library.bin, loaded: true, busy: false, error: String(error) };
  }
}

/** Lists the bin's next page, when it has one and no call runs. */
export async function listBinNext(): Promise<void> {
  const cursor = library.bin.next;
  if (cursor === null || library.bin.busy) return;
  const epoch = binEpoch;
  library.bin.busy = true;
  try {
    const answer = await invoke<RecyclePage>("library_recycle", { cursor });
    if (epoch !== binEpoch) return;
    library.bin = { items: mergeBin(library.bin.items, answer.items), next: answer.next, loaded: true, busy: false, error: null };
  } catch (error) {
    if (epoch !== binEpoch) return;
    library.bin = { ...library.bin, busy: false, error: String(error) };
  }
}

/** The restore of `key` follows its job: it asks, runs, or is over (done, failed, cancelled). */
function followRestore(job: Job, key?: string) {
  const at = key ?? Object.keys(library.restores).find((k) => library.restores[k].job === job.id);
  if (at === undefined) return;
  const restore = library.restores[at];
  if (!restore) return;
  if (job.state === "asking") {
    library.restores[at] = {
      ...restore,
      job: job.id,
      state: "asking",
      text: job.text,
      name: job.question?.name ?? restore.name,
    };
  } else if (job.state === "done" || job.state === "failed" || job.state === "cancelled") {
    delete library.restores[at];
  } else {
    library.restores[at] = { ...restore, job: job.id, state: "busy" };
  }
}

/**
 * Restores an item of the bin at `path`, or where it was. A done restore sends `recycle`, so the
 * bin lists again; one that failed (its text is the job's) lists the bin again here.
 */
async function restore(key: string, path: string | null, at: Restore) {
  library.restores[key] = { ...at, state: "busy" };
  const job = await change("library_restore", { key, path });
  if (job) followRestore(job, key);
  else delete library.restores[key];
  if (!job || job.state === "failed") await listBin();
}

/** The Restore button of a row. */
export function restoreItem(item: RecycleItem): Promise<void> {
  if (library.restores[item.key]) return Promise.resolve();
  return restore(item.key, null, { job: null, state: "busy", text: null, name: item.name, path: item.path });
}

/** The name typed for a restore that asks: null (Escape) is Cancel; an empty or the taken name keeps asking. */
export async function answerRestore(key: string, typed: string | null): Promise<void> {
  const at = library.restores[key];
  if (at?.state !== "asking") return;
  if (typed === null) return cancelRestore(key);
  const path = restorePath(at.path, typed, at.name);
  if (path === null) return;
  await restore(key, path, at);
}

/** Cancel of a restore that asks: Rust ends its job. */
export async function cancelRestore(key: string): Promise<void> {
  const at = library.restores[key];
  if (at?.state !== "asking" || at.job === null) return;
  delete library.restores[key];
  await run("library_cancel", { job: at.job });
}

/** Empty: removes every item of the bin for good. A done Empty sends `recycle`. */
export async function emptyBin(): Promise<void> {
  const job = await change("library_empty", {});
  if (job?.state !== "done") await listBin();
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
