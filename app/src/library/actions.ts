import type { Entry, FolderState, Job, UploadAsk } from "../lib/library-types";
import { formatBytes, fraction } from "../settings/format";
import { ROOTS, parentOf } from "./view";

// The view models of what the library window does with files: drops, moves between folders,
// the actions on a row and the job rows at the bottom. Pure functions; the components only
// wire them to the pointer and to Rust.

export type Point = { x: number; y: number };

/** What a drop on a folder that the app may not change says. */
export const NOT_WRITABLE = "This folder cannot be changed.";
/** What a drop says when no folder is under it. */
export const NO_FOLDER = "Drop the files on a folder.";

/**
 * The folder under a drop. Rust gives the drop position in physical pixels of the web view;
 * `hit` takes CSS pixels (as `document.elementFromPoint`) and answers the path of the folder
 * there (the nearest `[data-folder]`), or null.
 */
export function folderAt(point: Point, dpr: number, hit: (x: number, y: number) => string | null): string | null {
  const ratio = dpr > 0 ? dpr : 1;
  return hit(point.x / ratio, point.y / ratio);
}

/**
 * Whether files may go into the folder at `path`: a device tree or a folder inside one, with
 * no dot name on the way (the server's rule; a tree itself cannot be renamed, but takes files).
 */
export function takesFiles(path: string): boolean {
  const root = ROOTS.find((r) => path === r.path || path.startsWith(`${r.path}/`));
  if (!root) return false;
  return !path
    .slice(root.path.length)
    .split("/")
    .some((part) => part.startsWith("."));
}

export type DropPlan = { upload: string } | { refuse: string };

/**
 * Where a drop goes: the folder it was dropped on. A drop on no folder (the sidebar's notes,
 * the toolbar, the job area) goes nowhere; the shown folder's listing carries its own path.
 */
export function dropPlan(hit: string | null): DropPlan {
  if (hit === null) return { refuse: NO_FOLDER };
  return takesFiles(hit) ? { upload: hit } : { refuse: NOT_WRITABLE };
}

/** The arguments of `library_upload`; the answers to its questions add `packages` and `many`. */
export type UploadArgs = { dropId: number; folder: string; many?: boolean; packages?: boolean };
export type AskChoice = "yes" | "no" | "cancel";

/** A question of an upload with its buttons. */
export function askView(ask: UploadAsk): { text: string; buttons: { id: AskChoice; label: string }[] } {
  const buttons: { id: AskChoice; label: string }[] =
    ask.ask === "packages"
      ? [
          { id: "yes", label: "Upload them too" },
          { id: "no", label: "Leave them out" },
          { id: "cancel", label: "Cancel" },
        ]
      : [
          { id: "yes", label: "Upload" },
          { id: "cancel", label: "Cancel" },
        ];
  return { text: ask.text, buttons };
}

/** The next call of `library_upload` after an answer; null when the upload is cancelled. */
export function answerAsk(args: UploadArgs, ask: UploadAsk, choice: AskChoice): UploadArgs | null {
  if (choice === "cancel") return null;
  if (ask.ask === "packages") return { ...args, packages: choice === "yes" };
  return { ...args, many: true };
}

/** A row that can be dragged onto a folder. */
export type Dragged = { id: number; path: string; kind: "file" | "folder"; writable: boolean };

/**
 * The folder a dragged row moves into when it is let go over `over`, or null: not over a
 * folder, a folder that takes no files, the folder it is in already, the dragged folder itself
 * or a folder inside it, or a row that cannot be changed.
 */
export function moveTarget(dragged: Dragged, over: string | null): string | null {
  if (over === null || !dragged.writable || !takesFiles(over)) return null;
  if (over === parentOf(dragged.path)) return null;
  if (dragged.kind === "folder" && (over === dragged.path || over.startsWith(`${dragged.path}/`))) return null;
  return over;
}

/** How far the pointer moves before a press on a row becomes a drag. */
export const DRAG_SLOP = 4;

export type Drag = { dragged: Dragged; from: Point; active: boolean; over: string | null };

/** A press on a row: a drag once the pointer moves far enough; null for a row that cannot be changed. */
export function pressRow(dragged: Dragged, at: Point): Drag | null {
  if (!dragged.writable) return null;
  return { dragged, from: at, active: false, over: null };
}

/** The pointer moved to `at`, over the folder `over` (or none). */
export function dragTo(drag: Drag, at: Point, over: string | null): Drag {
  const active = drag.active || Math.hypot(at.x - drag.from.x, at.y - drag.from.y) >= DRAG_SLOP;
  return { ...drag, active, over: active ? moveTarget(drag.dragged, over) : null };
}

/** A move of the entry `id` at the path `from` into the folder `toFolder`. */
export type Move = { id: number; from: string; toFolder: string };

/** The pointer was let go: the move to make, if any. */
export function release(drag: Drag): Move | null {
  if (!drag.active || drag.over === null) return null;
  return { id: drag.dragged.id, from: drag.dragged.path, toFolder: drag.over };
}

/** The entry of `path` in any listing (the list's or the tree's); null for a device tree. */
export function entryAt(path: string, listings: Record<string, FolderState>): Entry | null {
  for (const listing of Object.values(listings)) {
    const found = listing.entries.find((e) => e.path === path);
    if (found) return found;
  }
  return null;
}

export type EntryActions = { newFolder: boolean; rename: boolean; delete: boolean; download: boolean; open: boolean };

/** A `.note` file (any case) opens in the review; the path is the one `note_open` takes. */
export function opensReview(entry: { path: string; kind: "file" | "folder" } | null): boolean {
  return entry?.kind === "file" && entry.path.toLowerCase().endsWith(".note");
}

/** The actions of the toolbar: New Folder in the shown folder, the rest on the picked entry. */
export function entryActions(entry: Entry | null, folder: string | null): EntryActions {
  return {
    newFolder: folder !== null && takesFiles(folder),
    rename: entry?.writable === true,
    delete: entry?.writable === true,
    download: entry?.kind === "file",
    open: opensReview(entry),
  };
}

/** The name typed for a rename or a new folder, trimmed; null when it is empty or unchanged. */
export function newName(typed: string, old: string): string | null {
  const name = typed.trim();
  return name === "" || name === old ? null : name;
}

/** How long a done or cancelled row stays. */
export const CLEAR_AFTER = 6_000;
/** How long a done download stays, for its Show in Finder. */
export const DOWNLOAD_CLEAR_AFTER = 60_000;

export type JobAction = "replace" | "keepBoth" | "skip" | "retry" | "cancel" | "reveal" | "clear";

export type JobRow = {
  id: number;
  job: Job;
  label: string;
  /** 0 to 1 while it runs or waits; null without a size or once it ended. */
  progress: number | null;
  text: string;
  asking: boolean;
  failed: boolean;
  actions: { id: JobAction; label: string }[];
};

export type JobGroup = {
  /** The drop the uploads came from; null for a job of its own. */
  drop: number | null;
  title: string | null;
  /** Two or more jobs of the drop can still be stopped. */
  cancelAll: boolean;
  /** The failed jobs of the drop, for Retry all; empty for fewer than two. */
  retryAll: number[];
  /** The waiting jobs of the drop that are not shown as rows. */
  more: number;
  rows: JobRow[];
};

const LIVE: readonly Job["state"][] = ["queued", "running", "waiting", "asking"];

function stateText(job: Job): string {
  if (job.text) return job.text;
  switch (job.state) {
    case "queued":
      return "Waiting to start.";
    case "running":
    case "waiting":
      if (job.total !== null && job.kind !== "change") return `${formatBytes(job.done)} of ${formatBytes(job.total)}`;
      return job.kind === "upload" ? "Uploading..." : job.kind === "download" ? "Downloading..." : "Working...";
    case "asking":
      return "Waiting for an answer.";
    case "done":
      return "Done.";
    case "failed":
      return "This did not work.";
    case "cancelled":
      return "Cancelled.";
  }
}

function actions(job: Job): JobRow["actions"] {
  const transfer = job.kind !== "change";
  if (job.state === "asking") {
    const answers: JobRow["actions"] = [];
    if (job.kind === "upload" && job.question) {
      if (job.question.canReplace) answers.push({ id: "replace", label: "Replace" });
      answers.push({ id: "keepBoth", label: "Keep both" }, { id: "skip", label: "Skip" });
    }
    return [...answers, { id: "cancel", label: "Cancel" }];
  }
  if (job.state === "failed") {
    return transfer
      ? [
          { id: "retry", label: "Retry" },
          { id: "cancel", label: "Cancel" },
        ]
      : [{ id: "clear", label: "Clear" }];
  }
  if (transfer && LIVE.includes(job.state)) return [{ id: "cancel", label: "Cancel" }];
  if (job.kind === "download" && job.state === "done" && job.result) return [{ id: "reveal", label: "Show in Finder" }];
  return [];
}

function jobRow(job: Job): JobRow {
  const moving = job.state === "queued" || job.state === "running" || job.state === "waiting";
  return {
    id: job.id,
    job,
    label: job.label,
    progress: moving && job.total !== null && job.kind !== "change" ? fraction(job.done, job.total) : null,
    text: stateText(job),
    asking: job.state === "asking",
    failed: job.state === "failed",
    actions: actions(job),
  };
}

/** Whether an ended row is cleared at `now`. */
function cleared(job: Job, ended: Record<number, number>, now: number): boolean {
  if (job.state !== "done" && job.state !== "cancelled") return false;
  const at = ended[job.id];
  if (at === undefined) return false;
  const keep = job.kind === "download" && job.state === "done" ? DOWNLOAD_CLEAR_AFTER : CLEAR_AFTER;
  return now - at >= keep;
}

/** How many waiting uploads of a drop show as rows; the rest are counted. */
export const QUEUED_SHOWN = 3;

/**
 * The job rows at the bottom of the window, in the order the jobs started: the uploads of a
 * drop together (with Cancel all while two or more of them can be stopped, and Retry all for
 * two or more failed ones), every other job on its own. Of a big drop only the first
 * {@link QUEUED_SHOWN} waiting uploads are rows; `more` counts the others. `ended` holds when
 * each job ended; done and cancelled rows go after a while. `totals` counts every job a drop
 * had, so its title stays the same while its done rows go.
 */
export function jobRows(
  jobs: Record<number, Job>,
  ended: Record<number, number>,
  now: number,
  totals: Record<number, number> = {},
): JobGroup[] {
  const groups: JobGroup[] = [];
  const byDrop = new Map<number, { group: JobGroup; jobs: Job[] }>();
  const ordered = Object.values(jobs).sort((a, b) => a.id - b.id);
  for (const job of ordered) {
    if (cleared(job, ended, now)) continue;
    if (job.dropId === null) {
      groups.push({ drop: null, title: null, cancelAll: false, retryAll: [], more: 0, rows: [jobRow(job)] });
      continue;
    }
    let found = byDrop.get(job.dropId);
    if (!found) {
      found = { group: { drop: job.dropId, title: null, cancelAll: false, retryAll: [], more: 0, rows: [] }, jobs: [] };
      byDrop.set(job.dropId, found);
      groups.push(found.group);
    }
    found.jobs.push(job);
  }
  for (const [drop, { group, jobs: all }] of byDrop) {
    let queued = 0;
    for (const job of all) {
      if (job.state === "queued" && ++queued > QUEUED_SHOWN) group.more += 1;
      else group.rows.push(jobRow(job));
    }
    const live = all.filter((j) => LIVE.includes(j.state) || j.state === "failed").length;
    const failed = all.filter((j) => j.state === "failed").map((j) => j.id);
    group.cancelAll = live >= 2;
    group.retryAll = failed.length >= 2 ? failed : [];
    const total = Math.max(totals[drop] ?? 0, all.length);
    group.title = total >= 2 ? `${total.toLocaleString("en-US")} dropped items` : null;
  }
  return groups;
}

/** The jobs to forget at `now`: their rows are cleared. */
export function clearedJobs(jobs: Record<number, Job>, ended: Record<number, number>, now: number): number[] {
  return Object.values(jobs)
    .filter((job) => cleared(job, ended, now))
    .map((job) => job.id);
}

/**
 * The folder to show after the folder `from` was renamed or moved to `to` (null: deleted):
 * the shown folder follows when it is `from` or inside it, and goes to the parent of a deleted
 * one.
 */
export function shownAfter(shown: string, from: string, to: string | null): string {
  if (shown !== from && !shown.startsWith(`${from}/`)) return shown;
  return to === null ? parentOf(from) : `${to}${shown.slice(from.length)}`;
}

/** A rename or a move of `from` to `to`, or its delete (`to` null). */
export type Change = { from: string; to: string | null };

/** What the window shows and has picked. */
export type Shown = { shown: string | null; picked: string | null };

/**
 * What the window shows after a change: the shown folder follows it (a folder shown anew has
 * no picked row), and the picked row follows its own rename or move and is let go when deleted.
 */
export function followChange(view: Shown, change: Change): Shown {
  if (view.shown !== null) {
    const shown = shownAfter(view.shown, change.from, change.to);
    if (shown !== view.shown) return { shown, picked: null };
  }
  return { shown: view.shown, picked: view.picked === change.from ? change.to : view.picked };
}
