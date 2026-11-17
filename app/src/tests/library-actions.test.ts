import { describe, expect, it } from "vitest";
import type { Entry, FolderState, Job } from "../lib/library-types";
import {
  CLEAR_AFTER,
  NO_FOLDER,
  NOT_WRITABLE,
  answerAsk,
  askView,
  dragTo,
  dropPlan,
  entryActions,
  entryAt,
  folderAt,
  followChange,
  jobRows,
  moveTarget,
  newName,
  pressRow,
  release,
  shownAfter,
  takesFiles,
} from "../library/actions";

function entry(path: string, kind: "file" | "folder" = "folder", writable = true, id = 1): Entry {
  return { id, path, name: path.slice(path.lastIndexOf("/") + 1), kind, size: kind === "file" ? 5 : null, md5: null, mtimeMs: 0, writable };
}

function job(id: number, patch: Partial<Job> = {}): Job {
  return {
    id,
    kind: "upload",
    label: `file ${id}.pdf`,
    done: 0,
    total: 1000,
    state: "queued",
    text: null,
    question: null,
    result: null,
    dropId: null,
    ...patch,
  };
}

describe("folderAt", () => {
  it("hit tests the drop in CSS pixels: physical pixels divided by the pixel ratio", () => {
    const seen: [number, number][] = [];
    const hit = (x: number, y: number) => {
      seen.push([x, y]);
      return x > 100 ? "NOTE/Note/Work" : null;
    };
    expect(folderAt({ x: 400, y: 120 }, 2, hit)).toBe("NOTE/Note/Work");
    expect(seen).toEqual([[200, 60]]);
    expect(folderAt({ x: 150, y: 10 }, 2, hit)).toBeNull();
    expect(folderAt({ x: 150, y: 10 }, 1, hit)).toBe("NOTE/Note/Work");
    // A ratio that is not known counts as 1.
    expect(folderAt({ x: 150, y: 10 }, 0, hit)).toBe("NOTE/Note/Work");
  });
});

describe("a drop", () => {
  it("goes into the folder it was dropped on", () => {
    expect(dropPlan("NOTE/Note/Work")).toEqual({ upload: "NOTE/Note/Work" });
    expect(dropPlan("INBOX")).toEqual({ upload: "INBOX" });
    // A device tree takes files, though it cannot be renamed.
    expect(dropPlan("DOCUMENT/Document")).toEqual({ upload: "DOCUMENT/Document" });
  });

  it("on a folder that is not writable says so", () => {
    expect(NOT_WRITABLE).toBe("This folder cannot be changed.");
    expect(dropPlan("NOTE")).toEqual({ refuse: NOT_WRITABLE });
    expect(dropPlan("NOTE/Other/x")).toEqual({ refuse: NOT_WRITABLE });
    expect(dropPlan("NOTE/Note/.hidden")).toEqual({ refuse: NOT_WRITABLE });
  });

  it("on no folder asks for one, even while a folder is shown", () => {
    expect(NO_FOLDER).toBe("Drop the files on a folder.");
    expect(dropPlan(null)).toEqual({ refuse: NO_FOLDER });
  });

  it("takes files only inside the device trees", () => {
    for (const yes of ["NOTE/Note", "NOTE/Note/a", "NOTE/MyStyle", "DOCUMENT/Document/b/c", "INBOX", "EXPORT/x", "SCREENSHOT"]) {
      expect(takesFiles(yes), yes).toBe(true);
    }
    for (const no of ["", "NOTE", "DOCUMENT", "NOTE/Notes", "INBOXES", "NOTE/Note/.x", "NOTE/Note/.x/y"]) {
      expect(takesFiles(no), no).toBe(false);
    }
  });
});

describe("the questions of an upload", () => {
  it("shows the package and the many question with their answers", () => {
    expect(askView({ ask: "packages", text: "Foo.app is a package. Upload what is in it too?" })).toEqual({
      text: "Foo.app is a package. Upload what is in it too?",
      buttons: [
        { id: "yes", label: "Upload them too" },
        { id: "no", label: "Leave them out" },
        { id: "cancel", label: "Cancel" },
      ],
    });
    expect(askView({ ask: "many", text: "Upload 1,234 files?" })).toEqual({
      text: "Upload 1,234 files?",
      buttons: [
        { id: "yes", label: "Upload" },
        { id: "cancel", label: "Cancel" },
      ],
    });
  });

  it("calls the upload again with the answer, and keeps the earlier answers", () => {
    const first = { dropId: 3, folder: "INBOX" };
    const packages = answerAsk(first, { ask: "packages", text: "" }, "no");
    expect(packages).toEqual({ dropId: 3, folder: "INBOX", packages: false });
    expect(answerAsk(packages!, { ask: "many", text: "" }, "yes")).toEqual({
      dropId: 3,
      folder: "INBOX",
      packages: false,
      many: true,
    });
    expect(answerAsk(first, { ask: "packages", text: "" }, "yes")).toEqual({ dropId: 3, folder: "INBOX", packages: true });
    expect(answerAsk(first, { ask: "many", text: "" }, "cancel")).toBeNull();
  });
});

describe("a pointer drag of a row", () => {
  const file = { id: 7, path: "NOTE/Note/Work/a.note", kind: "file" as const, writable: true };
  const folder = { id: 8, path: "NOTE/Note/Work", kind: "folder" as const, writable: true };

  it("onto a folder gives a move", () => {
    let drag = pressRow(file, { x: 10, y: 10 })!;
    expect(drag.active).toBe(false);
    // A small shake is a click, not a drag.
    drag = dragTo(drag, { x: 12, y: 11 }, "NOTE/Note/Home");
    expect(drag.active).toBe(false);
    expect(release(drag)).toBeNull();
    drag = dragTo(drag, { x: 30, y: 40 }, "NOTE/Note/Home");
    expect(drag).toMatchObject({ active: true, over: "NOTE/Note/Home" });
    expect(release(drag)).toEqual({ id: 7, from: "NOTE/Note/Work/a.note", toFolder: "NOTE/Note/Home" });
    // Once it moved, it stays a drag when it comes back near where it started.
    drag = dragTo(drag, { x: 11, y: 10 }, "DOCUMENT/Document");
    expect(release(drag)).toEqual({ id: 7, from: "NOTE/Note/Work/a.note", toFolder: "DOCUMENT/Document" });
  });

  it("onto the same folder, a folder that is not writable or nothing does nothing", () => {
    const drag = dragTo(pressRow(file, { x: 0, y: 0 })!, { x: 50, y: 0 }, null);
    expect(drag.active).toBe(true);
    expect(release(drag)).toBeNull();
    expect(release(dragTo(drag, { x: 60, y: 0 }, "NOTE/Note/Work"))).toBeNull();
    expect(release(dragTo(drag, { x: 60, y: 0 }, "NOTE"))).toBeNull();
    expect(moveTarget(file, "NOTE/Note/Work")).toBeNull();
    expect(moveTarget(file, "NOTE/Other")).toBeNull();
    expect(moveTarget(file, "INBOX")).toBe("INBOX");
  });

  it("of a folder not into itself, and not of what cannot be changed", () => {
    expect(moveTarget(folder, "NOTE/Note/Work")).toBeNull();
    expect(moveTarget(folder, "NOTE/Note/Work/Sub")).toBeNull();
    expect(moveTarget(folder, "NOTE/Note/Workshop")).toBe("NOTE/Note/Workshop");
    expect(moveTarget(folder, "NOTE/Note")).toBeNull();
    expect(moveTarget({ ...file, writable: false }, "INBOX")).toBeNull();
  });

  it("does not start on a row that cannot be changed", () => {
    expect(pressRow({ ...file, writable: false }, { x: 0, y: 0 })).toBeNull();
    expect(pressRow(file, { x: 0, y: 0 })).not.toBeNull();
  });
});

describe("the actions on a row", () => {
  const listings: Record<string, FolderState> = {
    "list:NOTE/Note/Work": {
      path: "NOTE/Note/Work",
      entries: [entry("NOTE/Note/Work/a.pdf", "file", true, 3), entry("NOTE/Note/Work/Sub", "folder", true, 4)],
      complete: true,
      loading: false,
      error: null,
    },
    "tree:NOTE/Note": {
      path: "NOTE/Note",
      entries: [entry("NOTE/Note/Work", "folder", true, 2)],
      complete: true,
      loading: false,
      error: null,
    },
  };

  it("finds the entry of a path in any listing", () => {
    expect(entryAt("NOTE/Note/Work/a.pdf", listings)?.id).toBe(3);
    expect(entryAt("NOTE/Note/Work", listings)?.id).toBe(2);
    expect(entryAt("NOTE/Note", listings)).toBeNull();
  });

  it("offers New Folder in a folder that takes files, and the rest for an entry that can be changed", () => {
    expect(entryActions(entry("NOTE/Note/Work/a.pdf", "file"), "NOTE/Note/Work")).toEqual({
      newFolder: true,
      rename: true,
      delete: true,
      download: true,
    });
    expect(entryActions(entry("NOTE/Note/Work/Sub"), "NOTE/Note/Work")).toEqual({
      newFolder: true,
      rename: true,
      delete: true,
      download: false,
    });
    // A device tree itself: New Folder in it, nothing on it.
    expect(entryActions(null, "NOTE/Note")).toEqual({ newFolder: true, rename: false, delete: false, download: false });
    expect(entryActions(null, null)).toEqual({ newFolder: false, rename: false, delete: false, download: false });
    // A file that cannot be changed can still be downloaded.
    expect(entryActions(entry("NOTE/Note/Work/b.pdf", "file", false), "NOTE/Note/Work")).toEqual({
      newFolder: true,
      rename: false,
      delete: false,
      download: true,
    });
  });

  it("takes a typed name only when it is new", () => {
    expect(newName("  Plans  ", "Work")).toBe("Plans");
    expect(newName("Work", "Work")).toBeNull();
    expect(newName("   ", "Work")).toBeNull();
    expect(newName("work", "Work")).toBe("work");
  });
});

describe("the shown folder after a change", () => {
  it("follows a rename or a move of it or of a folder above it, and goes up after a delete", () => {
    expect(shownAfter("NOTE/Note/Work/A", "NOTE/Note/Work", "NOTE/Note/Jobs")).toBe("NOTE/Note/Jobs/A");
    expect(shownAfter("NOTE/Note/Work", "NOTE/Note/Work", "INBOX/Work")).toBe("INBOX/Work");
    expect(shownAfter("NOTE/Note/Workshop", "NOTE/Note/Work", "NOTE/Note/Jobs")).toBe("NOTE/Note/Workshop");
    expect(shownAfter("NOTE/Note/Work/A", "NOTE/Note/Work", null)).toBe("NOTE/Note");
    expect(shownAfter("INBOX", "NOTE/Note/Work", null)).toBe("INBOX");
  });

  it("moves the shown folder and the picked row along with the change", () => {
    // The shown folder follows; a new folder shown has no picked row.
    expect(followChange({ shown: "NOTE/Note/Work/A", picked: "NOTE/Note/Work/A/x.pdf" }, { from: "NOTE/Note/Work", to: "INBOX/Work" })).toEqual({
      shown: "INBOX/Work/A",
      picked: null,
    });
    // The picked row itself was renamed in the shown folder.
    expect(followChange({ shown: "NOTE/Note", picked: "NOTE/Note/a.pdf" }, { from: "NOTE/Note/a.pdf", to: "NOTE/Note/b.pdf" })).toEqual({
      shown: "NOTE/Note",
      picked: "NOTE/Note/b.pdf",
    });
    // A deleted picked row is no longer picked; another picked row stays.
    expect(followChange({ shown: "NOTE/Note", picked: "NOTE/Note/a.pdf" }, { from: "NOTE/Note/a.pdf", to: null })).toEqual({
      shown: "NOTE/Note",
      picked: null,
    });
    expect(followChange({ shown: "NOTE/Note", picked: "NOTE/Note/c.pdf" }, { from: "NOTE/Note/a.pdf", to: null })).toEqual({
      shown: "NOTE/Note",
      picked: "NOTE/Note/c.pdf",
    });
    expect(followChange({ shown: null, picked: null }, { from: "NOTE/Note/a.pdf", to: "INBOX/a.pdf" })).toEqual({ shown: null, picked: null });
  });
});

describe("jobRows", () => {
  it("shows the progress of a running job", () => {
    const [group] = jobRows({ 1: job(1, { state: "running", done: 250, total: 1000 }) }, {}, 0);
    expect(group.rows[0]).toMatchObject({
      id: 1,
      label: "file 1.pdf",
      progress: 0.25,
      text: "250 bytes of 1.0 KB",
      actions: [{ id: "cancel", label: "Cancel" }],
    });
    // A job without a size has no bar.
    const [folder] = jobRows({ 2: job(2, { state: "running", total: null }) }, {}, 0);
    expect(folder.rows[0]).toMatchObject({ progress: null, text: "Uploading..." });
    const [queued] = jobRows({ 3: job(3) }, {}, 0);
    expect(queued.rows[0]).toMatchObject({ progress: 0, text: "Waiting to start.", actions: [{ id: "cancel", label: "Cancel" }] });
    const [waiting] = jobRows({ 4: job(4, { state: "waiting", text: "The Supernote is syncing. Notedown waits." }) }, {}, 0);
    expect(waiting.rows[0]).toMatchObject({ text: "The Supernote is syncing. Notedown waits." });
  });

  it("asks the clash question: Replace only when it can, Keep Both and Skip", () => {
    const asking = job(1, {
      state: "asking",
      text: "There is already a.pdf here.",
      question: { name: "a.pdf", existing: "a.pdf", canReplace: true },
    });
    const [group] = jobRows({ 1: asking }, {}, 0);
    expect(group.rows[0]).toMatchObject({ text: "There is already a.pdf here.", asking: true });
    expect(group.rows[0].actions.map((a) => a.id)).toEqual(["replace", "keepBoth", "skip", "cancel"]);
    expect(group.rows[0].actions.map((a) => a.label)).toEqual(["Replace", "Keep both", "Skip", "Cancel"]);
    const folder = { ...asking, question: { name: "a", existing: "a", canReplace: false } };
    expect(jobRows({ 1: folder }, {}, 0)[0].rows[0].actions.map((a) => a.id)).toEqual(["keepBoth", "skip", "cancel"]);
    // A restore that asks for a name is answered in the recycle bin; here it can be cancelled.
    const restore = { ...asking, kind: "change" as const, question: { name: "a", existing: "a", canReplace: false } };
    expect(jobRows({ 1: restore }, {}, 0)[0].rows[0].actions.map((a) => a.id)).toEqual(["cancel"]);
  });

  it("offers Retry and Cancel on a failed transfer, and Clear on a failed change", () => {
    const failed = job(1, { state: "failed", text: "The connection was cut." });
    expect(jobRows({ 1: failed }, {}, 0)[0].rows[0]).toMatchObject({
      text: "The connection was cut.",
      failed: true,
      actions: [
        { id: "retry", label: "Retry" },
        { id: "cancel", label: "Cancel" },
      ],
    });
    const download = { ...failed, kind: "download" as const };
    expect(jobRows({ 1: download }, {}, 0)[0].rows[0].actions.map((a) => a.id)).toEqual(["retry", "cancel"]);
    const change = { ...failed, kind: "change" as const, text: "This name is not allowed." };
    expect(jobRows({ 1: change }, {}, 0)[0].rows[0].actions).toEqual([{ id: "clear", label: "Clear" }]);
    // A running change cannot be stopped.
    const running = job(2, { kind: "change", state: "running", total: null, label: "Delete a.pdf" });
    expect(jobRows({ 2: running }, {}, 0)[0].rows[0]).toMatchObject({ actions: [], text: "Working..." });
  });

  it("groups the uploads of a drop with Cancel all", () => {
    const jobs = {
      1: job(1, { dropId: 5, state: "running" }),
      2: job(2, { dropId: 5 }),
      3: job(3, { kind: "download", state: "running", label: "b.pdf" }),
      4: job(4, { dropId: 6 }),
      5: job(5, { dropId: 5, state: "done", done: 1000 }),
    };
    const groups = jobRows(jobs, { 5: 0 }, 0);
    expect(groups.map((g) => [g.drop, g.rows.map((r) => r.id), g.cancelAll])).toEqual([
      [5, [1, 2, 5], true],
      [null, [3], false],
      [6, [4], false],
    ]);
    expect(groups[0].title).toBe("3 dropped items");
    expect(groups[2].title).toBeNull();
    // Cancel all goes away when only one job of the drop is left to stop.
    const one = jobRows({ ...jobs, 2: job(2, { dropId: 5, state: "done" }) }, { 2: 0, 5: 0 }, 0);
    expect(one[0].cancelAll).toBe(false);
  });

  it("shows only the first waiting uploads of a big drop, and Retry all for its failed ones", () => {
    const jobs: Record<number, Job> = {};
    for (let id = 1; id <= 1000; id++) jobs[id] = job(id, { dropId: 9 });
    jobs[1] = job(1, { dropId: 9, state: "running" });
    jobs[2] = job(2, { dropId: 9, state: "failed", text: "The connection was cut." });
    jobs[500] = job(500, { dropId: 9, state: "asking", question: { name: "x", existing: "x", canReplace: true } });
    jobs[501] = job(501, { dropId: 9, state: "failed", text: "The connection was cut." });
    const [group] = jobRows(jobs, {}, 0);
    expect(group.rows.map((r) => r.id)).toEqual([1, 2, 3, 4, 5, 500, 501]);
    expect(group.more).toBe(993);
    expect(group.title).toBe("1,000 dropped items");
    expect(group.cancelAll).toBe(true);
    expect(group.retryAll).toEqual([2, 501]);
    // One failed job has its own Retry.
    const one = jobRows({ 1: job(1, { dropId: 9, state: "failed" }), 2: job(2, { dropId: 9 }) }, {}, 0);
    expect(one[0]).toMatchObject({ more: 0, retryAll: [] });
  });

  it("names a drop by all its items, also when done rows of it were cleared", () => {
    const left = { 3: job(3, { dropId: 5, state: "running" }) };
    expect(jobRows(left, {}, 0, { 5: 3 })[0].title).toBe("3 dropped items");
    // Without a total the jobs there are counted.
    expect(jobRows(left, {}, 0)[0].title).toBeNull();
    expect(jobRows(left, {}, 0, { 5: 1 })[0].title).toBeNull();
  });

  it("clears done and cancelled rows after a while; a done download shows Show in Finder for longer", () => {
    const jobs = {
      1: job(1, { state: "done", done: 1000 }),
      2: job(2, { state: "cancelled" }),
      3: job(3, { kind: "download", state: "done", done: 1000, result: "/Users/me/Downloads/b.pdf" }),
      4: job(4, { state: "failed", text: "Files up to 1 GB." }),
      5: job(5, { state: "done", text: "Skipped." }),
    };
    const ended = { 1: 1000, 2: 1000, 3: 1000, 5: 1000 };
    const soon = jobRows(jobs, ended, 1000 + CLEAR_AFTER - 1).flatMap((g) => g.rows);
    expect(soon.map((r) => r.id)).toEqual([1, 2, 3, 4, 5]);
    expect(soon[0]).toMatchObject({ text: "Done.", progress: null, actions: [] });
    expect(soon[1]).toMatchObject({ text: "Cancelled.", actions: [] });
    expect(soon[2]).toMatchObject({ text: "Done.", actions: [{ id: "reveal", label: "Show in Finder" }] });
    expect(soon[4]).toMatchObject({ text: "Skipped." });
    const later = jobRows(jobs, ended, 1000 + CLEAR_AFTER).flatMap((g) => g.rows);
    expect(later.map((r) => r.id)).toEqual([3, 4]);
    // A failed job stays until it is retried, cancelled or cleared.
    const much = jobRows(jobs, ended, 1000 + 100 * CLEAR_AFTER).flatMap((g) => g.rows);
    expect(much.map((r) => r.id)).toEqual([4]);
  });
});
