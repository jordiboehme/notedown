import { beforeEach, describe, expect, it, vi } from "vitest";
import type { FolderState, Job, LibraryEvent, Listing, RecycleItem, RecyclePage } from "../lib/library-types";

type Handler = (event: { payload: unknown }) => void;
const handlers = new Map<string, Handler>();
const unlisten = vi.fn();
const calls: { command: string; args: Record<string, unknown> | undefined }[] = [];
const answers = new Map<string, ((args: Record<string, unknown> | undefined) => Promise<unknown>)[]>();

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (name: string, handler: Handler) => {
    handlers.set(name, handler);
    return unlisten;
  }),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn((command: string, args?: Record<string, unknown>) => {
    calls.push({ command, args });
    const next = answers.get(command)?.shift();
    return next ? next(args) : Promise.resolve(null);
  }),
}));

const store = await import("../lib/library.svelte");
const { library, applyEvent, startLibrary, listFolder, listNext, isBusy, showListings, resetLibrary, loadView, chooseView } = store;
const { setDropTarget, answerUpload, jobAction, moveEntry, renameEntry, newFolder, deleteEntry, downloadEntry, clearJobs, retryJobs } = store;
const { moveRow, renameRow, makeFolder, deleteRow } = store;
const { listBin, listBinNext, restoreItem, answerRestore, cancelRestore, emptyBin } = store;
const { followChange, jobRows } = await import("../library/actions");

/** The next call of `command` answers what `answer` gives; it waits until `release` is called. */
function answerLater(command: string) {
  let release: (value: unknown) => void = () => {};
  let fail: (error: unknown) => void = () => {};
  const list = answers.get(command) ?? [];
  list.push(
    () =>
      new Promise((resolve, reject) => {
        release = resolve;
        fail = reject;
      }),
  );
  answers.set(command, list);
  return { release: (value: unknown) => release(value), fail: (error: unknown) => fail(error) };
}

function answerNow(command: string, value: unknown) {
  const list = answers.get(command) ?? [];
  list.push(() => Promise.resolve(value));
  answers.set(command, list);
}

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

function folder(path: string, names: string[], complete: boolean): FolderState {
  return {
    path,
    entries: names.map((name, i) => ({
      id: i + 1,
      path: `${path}/${name}`,
      name,
      kind: "file",
      size: 1,
      md5: null,
      mtimeMs: 0,
      writable: true,
    })),
    complete,
    loading: false,
    error: null,
  };
}

const job: Job = {
  id: 4,
  kind: "download",
  label: "a.pdf",
  done: 0,
  total: 10,
  state: "queued",
  text: null,
  question: null,
  result: null,
  dropId: null,
};

beforeEach(() => {
  calls.length = 0;
  answers.clear();
  handlers.clear();
  resetLibrary();
});

describe("the library store", () => {
  it("applies each LibraryEvent", () => {
    applyEvent({ type: "state", state: "unreachable", text: "The server does not answer.", page: "server" });
    expect(library.view).toEqual({ state: "unreachable", text: "The server does not answer.", page: "server" });

    applyEvent({ type: "folder", folder: folder("INBOX", ["a"], true), foldersOnly: false, next: null });
    applyEvent({ type: "folder", folder: folder("INBOX", [], true), foldersOnly: true, next: null });
    // The tree's listing and the list's of the same path stay apart.
    expect(library.listings["list:INBOX"].entries).toHaveLength(1);
    expect(library.listings["tree:INBOX"].entries).toHaveLength(0);

    applyEvent({ type: "job", job });
    applyEvent({ type: "job", job: { ...job, state: "running", done: 5 } });
    expect(library.jobs[4]).toMatchObject({ state: "running", done: 5 });

    const recycle = library.recycle;
    applyEvent({ type: "recycle" });
    expect(library.recycle).toBe(recycle + 1);

    // A state change without a new binding keeps the view.
    applyEvent({ type: "state", state: "ready", text: null, page: null });
    expect(library.listings["list:INBOX"]).toBeDefined();

    // A new binding drops what is shown.
    const generation = library.generation;
    applyEvent({ type: "relist" });
    expect(library.listings).toEqual({});
    expect(library.next).toEqual({});
    expect(library.generation).toBe(generation + 1);
    expect(library.jobs[4]).toBeDefined();

    // With no window to find its folder, a drop is let go, so Rust forgets its paths.
    applyEvent({ type: "drop", dropId: 9, position: { x: 1, y: 2 }, count: 1 });
    expect(calls).toContainEqual({ command: "library_cancel_drop", args: { dropId: 9 } });
  });

  it("forgets the cursor of a folder that an event shows complete", async () => {
    answerNow("library_list", { folder: folder("INBOX", ["a"], false), next: "t1:a" } satisfies Listing);
    await listFolder("INBOX", false);
    expect(library.next["list:INBOX"]).toBe("t1:a");
    applyEvent({ type: "folder", folder: folder("INBOX", ["a", "b"], true), foldersOnly: false, next: null });
    expect(library.next["list:INBOX"]).toBeNull();
  });

  it("pages on with the cursor of a folder Rust listed again by itself", async () => {
    showListings([{ path: "INBOX", foldersOnly: false }]);
    answerNow("library_list", { folder: folder("INBOX", ["a", "b"], true), next: null } satisfies Listing);
    await listFolder("INBOX", false);
    // A gap: Rust lists page 1 again and sends it with its cursor.
    applyEvent({ type: "folder", folder: folder("INBOX", ["a"], false), foldersOnly: false, next: "t2:a" });
    expect(library.next["list:INBOX"]).toBe("t2:a");
    answerNow("library_list", { folder: folder("INBOX", ["a", "b"], true), next: null } satisfies Listing);
    await listNext("INBOX", false);
    expect(calls.at(-1)).toEqual({
      command: "library_list",
      args: { path: "INBOX", cursor: "t2:a", foldersOnly: false },
    });
  });

  it("starts with the state Rust has, unless an event came first", async () => {
    const first = answerLater("library_state");
    const stop = await startLibrary();
    handlers.get("library")!({
      payload: { type: "state", state: "loggedOut", text: "Log in to see your library.", page: "account" },
    } satisfies { payload: LibraryEvent });
    first.release({ state: "ready", text: null, page: null });
    await flush();
    expect(library.view?.state).toBe("loggedOut");
    stop();
    expect(unlisten).toHaveBeenCalled();

    resetLibrary();
    answerNow("library_state", { state: "ready", text: null, page: null });
    await startLibrary();
    await flush();
    expect(library.view?.state).toBe("ready");
  });

  it("lists a folder page by page, one call at a time", async () => {
    answerNow("library_list", { folder: folder("INBOX", ["a"], false), next: "t1:a" } satisfies Listing);
    await listFolder("INBOX", false);
    expect(calls.at(-1)).toEqual({
      command: "library_list",
      args: { path: "INBOX", cursor: null, foldersOnly: false },
    });

    const second = answerLater("library_list");
    const going = listNext("INBOX", false);
    expect(isBusy("INBOX", false)).toBe(true);
    void listNext("INBOX", false);
    expect(calls.filter((c) => c.command === "library_list")).toHaveLength(2);
    second.release({ folder: folder("INBOX", ["a", "b"], true), next: null } satisfies Listing);
    await going;
    expect(isBusy("INBOX", false)).toBe(false);
    expect(library.listings["list:INBOX"].entries.map((e) => e.name)).toEqual(["a", "b"]);
    expect(library.next["list:INBOX"]).toBeNull();
    // Complete: no more calls.
    await listNext("INBOX", false);
    expect(calls.filter((c) => c.command === "library_list")).toHaveLength(2);
  });

  it("drops an answer that comes after a new binding", async () => {
    const late = answerLater("library_list");
    const going = listFolder("EXPORT", true);
    applyEvent({ type: "relist" });
    expect(isBusy("EXPORT", true)).toBe(false);
    late.release({ folder: folder("EXPORT", ["old"], true), next: null } satisfies Listing);
    await going;
    expect(library.listings["tree:EXPORT"]).toBeUndefined();
  });

  it("says why a folder could not be listed, but not for a new binding", async () => {
    const failing = answerLater("library_list");
    const going = listFolder("INBOX", false);
    failing.fail("The server does not answer.");
    await going;
    expect(library.listings["list:INBOX"]).toMatchObject({ error: "The server does not answer.", loading: false });

    resetLibrary();
    const changed = answerLater("library_list");
    const again = listFolder("INBOX", false);
    changed.fail("The library changed. List again.");
    await again;
    expect(library.listings["list:INBOX"]).toBeUndefined();
  });

  it("takes no listing that is not shown, so a folder opened again is listed fresh", async () => {
    showListings([]);
    // A follower event that was on its way when the folder closed.
    applyEvent({ type: "folder", folder: folder("EXPORT", ["a"], false), foldersOnly: true, next: null });
    expect(library.listings["tree:EXPORT"]).toBeUndefined();
    showListings([{ path: "EXPORT", foldersOnly: true }]);
    expect(library.listings["tree:EXPORT"]).toBeUndefined();

    // An answer that comes after the folder closed.
    const late = answerLater("library_list");
    const going = listFolder("EXPORT", true);
    showListings([]);
    late.release({ folder: folder("EXPORT", ["a"], true), next: null } satisfies Listing);
    await going;
    expect(library.listings["tree:EXPORT"]).toBeUndefined();
    const failing = answerLater("library_list");
    const again = listFolder("EXPORT", true);
    failing.fail("The server does not answer.");
    await again;
    expect(library.listings["tree:EXPORT"]).toBeUndefined();
    expect(isBusy("EXPORT", true)).toBe(false);
  });

  it("tells Rust the shown folders only when they change", () => {
    showListings([
      { path: "NOTE/Note", foldersOnly: true },
      { path: "INBOX", foldersOnly: false },
    ]);
    showListings([
      { path: "NOTE/Note", foldersOnly: true },
      { path: "INBOX", foldersOnly: false },
    ]);
    const shown = calls.filter((c) => c.command === "library_shown");
    expect(shown).toEqual([
      {
        command: "library_shown",
        args: {
          listings: [
            { path: "NOTE/Note", foldersOnly: true },
            { path: "INBOX", foldersOnly: false },
          ],
          binShown: false,
        },
      },
    ]);
    // A listing that is not shown any more is not followed by Rust: the store forgets it, so
    // opening the folder again lists it again.
    applyEvent({ type: "folder", folder: folder("NOTE/Note", [], true), foldersOnly: true, next: null });
    applyEvent({ type: "folder", folder: folder("INBOX", ["a"], false), foldersOnly: false, next: null });
    library.next["list:INBOX"] = "t1:a";
    showListings([{ path: "INBOX", foldersOnly: false }]);
    expect(Object.keys(library.listings)).toEqual(["list:INBOX"]);
    expect(library.next["list:INBOX"]).toBe("t1:a");
    showListings([]);
    expect(library.listings).toEqual({});
    expect(library.next).toEqual({});

    // After a new binding Rust starts over: tell it again.
    applyEvent({ type: "relist" });
    showListings([{ path: "INBOX", foldersOnly: false }]);
    showListings([{ path: "INBOX", foldersOnly: false }]);
    expect(calls.filter((c) => c.command === "library_shown")).toHaveLength(4);

    // The bin is told too: the same folders with the bin shown are a change.
    showListings([{ path: "INBOX", foldersOnly: false }], true);
    showListings([{ path: "INBOX", foldersOnly: false }], true);
    const last = calls.filter((c) => c.command === "library_shown");
    expect(last).toHaveLength(5);
    expect(last[4].args).toEqual({ listings: [{ path: "INBOX", foldersOnly: false }], binShown: true });
  });
});

describe("the view of the window", () => {
  beforeEach(() => {
    resetLibrary();
    calls.length = 0;
  });

  it("starts in the saved view", async () => {
    expect(library.viewMode).toBe("tree");
    answerNow("get_config", { startView: "columns" });
    await loadView();
    expect(library.viewMode).toBe("columns");
    expect(calls.filter((c) => c.command === "get_config")).toHaveLength(1);
  });

  it("keeps the tree when the config cannot be read", async () => {
    answers.set("get_config", [() => Promise.reject("no")]);
    await loadView();
    expect(library.viewMode).toBe("tree");
  });

  it("switches at once and saves the choice", async () => {
    answerNow("set_start_view", { startView: "columns" });
    const saving = chooseView("columns");
    expect(library.viewMode).toBe("columns");
    await saving;
    expect(calls.find((c) => c.command === "set_start_view")?.args).toEqual({ startView: "columns" });
  });

  it("does not save a view that is already shown", async () => {
    await chooseView("tree");
    expect(calls.some((c) => c.command === "set_start_view")).toBe(false);
  });

  it("stays in the chosen view when saving fails", async () => {
    answers.set("set_start_view", [() => Promise.reject("disk full")]);
    await chooseView("columns");
    expect(library.viewMode).toBe("columns");
  });

  it("does not let a late config read undo a choice", async () => {
    const read = answerLater("get_config");
    const loading = loadView();
    await chooseView("columns");
    read.release({ startView: "tree" });
    await loading;
    expect(library.viewMode).toBe("columns");
  });

  it("keeps one save in flight and then saves the latest choice", async () => {
    const first = answerLater("set_start_view");
    const one = chooseView("columns");
    const two = chooseView("tree");
    await flush();
    expect(calls.filter((c) => c.command === "set_start_view")).toHaveLength(1);
    first.release({});
    await Promise.all([one, two]);
    const saves = calls.filter((c) => c.command === "set_start_view").map((c) => c.args);
    expect(saves).toEqual([{ startView: "columns" }, { startView: "tree" }]);
    expect(library.viewMode).toBe("tree");
  });

  it("does not save again when the latest choice is the one that was saved", async () => {
    const first = answerLater("set_start_view");
    const all = [chooseView("columns"), chooseView("tree"), chooseView("columns")];
    first.release({});
    await Promise.all(all);
    expect(calls.filter((c) => c.command === "set_start_view")).toHaveLength(1);
  });
});

describe("drops", () => {
  const drop = (dropId: number): LibraryEvent => ({ type: "drop", dropId, position: { x: 40, y: 80 }, count: 2 });
  const uploads = () => calls.filter((c) => c.command === "library_upload").map((c) => c.args);
  const cancels = () => calls.filter((c) => c.command === "library_cancel_drop").map((c) => c.args);

  it("uploads into the folder the window finds at the drop position", async () => {
    const seen: { x: number; y: number }[] = [];
    setDropTarget((position) => {
      seen.push(position);
      return "NOTE/Note/Work";
    });
    answerNow("library_upload", null);
    applyEvent(drop(3));
    await flush();
    expect(seen).toEqual([{ x: 40, y: 80 }]);
    expect(uploads()).toEqual([{ dropId: 3, folder: "NOTE/Note/Work" }]);
    expect(cancels()).toEqual([]);
    expect(library.asks).toEqual([]);
  });

  it("on a folder that is not writable says so and lets the drop go", async () => {
    setDropTarget(() => "NOTE");
    applyEvent(drop(4));
    await flush();
    expect(library.notice).toBe("This folder cannot be changed.");
    expect(uploads()).toEqual([]);
    expect(cancels()).toEqual([{ dropId: 4 }]);
  });

  it("without a folder asks for one and lets the drop go", async () => {
    setDropTarget(() => null);
    applyEvent(drop(5));
    await flush();
    expect(library.notice).toBe("Drop the files on a folder.");
    expect(cancels()).toEqual([{ dropId: 5 }]);
  });

  it("shows the questions of the upload and answers them", async () => {
    setDropTarget(() => "INBOX");
    answerNow("library_upload", { ask: "packages", text: "Foo.app is a package. Upload what is in it too?" });
    answerNow("library_upload", { ask: "many", text: "Upload 1,234 files?" });
    answerNow("library_upload", null);
    applyEvent(drop(6));
    await flush();
    expect(library.asks).toEqual([
      { dropId: 6, folder: "INBOX", ask: { ask: "packages", text: "Foo.app is a package. Upload what is in it too?" } },
    ]);
    await answerUpload(6, "no");
    expect(library.asks).toMatchObject([{ dropId: 6, packages: false, ask: { ask: "many" } }]);
    await answerUpload(6, "yes");
    expect(library.asks).toEqual([]);
    expect(uploads()).toEqual([
      { dropId: 6, folder: "INBOX" },
      { dropId: 6, folder: "INBOX", packages: false },
      { dropId: 6, folder: "INBOX", packages: false, many: true },
    ]);
    expect(cancels()).toEqual([]);
  });

  it("forgets the questions on a new binding: Rust forgot their drops", async () => {
    setDropTarget(() => "INBOX");
    answerNow("library_upload", { ask: "many", text: "Upload 1,234 files?" });
    applyEvent(drop(9));
    await flush();
    expect(library.asks).toHaveLength(1);
    applyEvent({ type: "relist" });
    expect(library.asks).toEqual([]);
    // Rust forgot the drop already: nothing is let go.
    expect(cancels()).toEqual([]);
  });

  it("lets the drop go when a question is cancelled or the upload fails", async () => {
    setDropTarget(() => "INBOX");
    answerNow("library_upload", { ask: "many", text: "Upload 1,234 files?" });
    applyEvent(drop(7));
    await flush();
    await answerUpload(7, "cancel");
    expect(library.asks).toEqual([]);
    expect(cancels()).toEqual([{ dropId: 7 }]);

    answers.set("library_upload", [() => Promise.reject("There is nothing in the drop to upload.")]);
    applyEvent(drop(8));
    await flush();
    expect(library.notice).toBe("There is nothing in the drop to upload.");
    expect(cancels()).toEqual([{ dropId: 7 }, { dropId: 8 }]);
  });
});

describe("jobs", () => {
  const upload: Job = { ...job, id: 11, kind: "upload", dropId: 2 };

  it("notes when a job ended and clears it later", () => {
    vi.useFakeTimers();
    try {
      vi.setSystemTime(5000);
      applyEvent({ type: "job", job: { ...upload, state: "running" } });
      expect(library.ended[11]).toBeUndefined();
      applyEvent({ type: "job", job: { ...upload, state: "done" } });
      expect(library.ended[11]).toBe(5000);
      // A later event of the same end keeps the first time.
      vi.setSystemTime(6000);
      applyEvent({ type: "job", job: { ...upload, state: "done" } });
      expect(library.ended[11]).toBe(5000);
      clearJobs(5000 + 60_000 * 10);
      expect(library.jobs[11]).toBeUndefined();
      expect(library.ended[11]).toBeUndefined();
    } finally {
      vi.useRealTimers();
    }
  });

  it("keeps a job that ended when an older event of it comes late", () => {
    const moved: Job = { ...job, id: 30, kind: "change", total: null, state: "done" };
    applyEvent({ type: "job", job: moved });
    applyEvent({ type: "job", job: { ...moved, state: "running" } });
    expect(library.jobs[30].state).toBe("done");
    // A failed upload starts again on Retry.
    applyEvent({ type: "job", job: { ...upload, state: "failed" } });
    applyEvent({ type: "job", job: { ...upload, state: "queued" } });
    expect(library.jobs[11].state).toBe("queued");
  });

  it("sends each action of a job row to Rust", async () => {
    applyEvent({ type: "job", job: upload });
    await jobAction(upload, "replace");
    await jobAction(upload, "keepBoth");
    await jobAction(upload, "skip");
    await jobAction(upload, "retry");
    await jobAction(upload, "cancel");
    await jobAction(upload, "cancelAll");
    await jobAction({ ...job, state: "done", result: "/Users/me/Downloads/a.pdf" }, "reveal");
    expect(calls.map((c) => [c.command, c.args])).toEqual([
      ["library_answer", { job: 11, choice: "replace" }],
      ["library_answer", { job: 11, choice: "keepBoth" }],
      ["library_answer", { job: 11, choice: "skip" }],
      ["library_retry", { job: 11 }],
      ["library_cancel", { job: 11 }],
      ["library_cancel_drop", { dropId: 2 }],
      ["library_reveal", { path: "/Users/me/Downloads/a.pdf" }],
    ]);
    calls.length = 0;
    await retryJobs([11, 12]);
    expect(calls.map((c) => [c.command, c.args])).toEqual([
      ["library_retry", { job: 11 }],
      ["library_retry", { job: 12 }],
    ]);
    await jobAction(upload, "clear");
    expect(library.jobs[11]).toBeUndefined();
  });

  it("keeps the title of a drop while its done rows are cleared", () => {
    for (const id of [41, 42, 43]) applyEvent({ type: "job", job: { ...upload, id, dropId: 9, state: "running" } });
    // A later event of a job already counted counts once.
    applyEvent({ type: "job", job: { ...upload, id: 41, dropId: 9, state: "done" } });
    applyEvent({ type: "job", job: { ...upload, id: 42, dropId: 9, state: "done" } });
    const title = () => jobRows(library.jobs, library.ended, Date.now() + 60_000 * 10, library.dropTotals)[0]?.title;
    expect(library.dropTotals[9]).toBe(3);
    clearJobs(Date.now() + 60_000 * 10);
    expect(Object.keys(library.jobs).map(Number)).toEqual([43]);
    expect(title()).toBe("3 dropped items");
    // Once the last job of the drop is gone, so is its total.
    applyEvent({ type: "job", job: { ...upload, id: 43, dropId: 9, state: "done" } });
    clearJobs(Date.now() + 60_000 * 10);
    expect(library.dropTotals[9]).toBeUndefined();
  });

  it("shows the error of an action", async () => {
    answers.set("library_reveal", [() => Promise.reject("This file is not there anymore.")]);
    await jobAction({ ...job, state: "done", result: "/x" }, "reveal");
    expect(library.notice).toBe("This file is not there anymore.");
  });

  it("runs the file actions as jobs and keeps the job each answers", async () => {
    const answered = (id: number, label: string, state: Job["state"] = "done"): Job => ({
      ...job,
      id,
      kind: "change",
      label,
      total: null,
      state,
    });
    answerNow("library_move", answered(20, "Move a.pdf to Home"));
    answerNow("library_rename", answered(21, "Rename a.pdf to b.pdf"));
    answerNow("library_new_folder", answered(22, "New folder Plans", "failed"));
    answerNow("library_delete", answered(23, "Delete b.pdf"));
    answerNow("library_download", { ...job, id: 24 });
    await moveEntry(7, "NOTE/Note/Home");
    await renameEntry(7, "b.pdf");
    await newFolder("NOTE/Note", "Plans");
    await deleteEntry(7);
    await downloadEntry(7);
    expect(calls.map((c) => [c.command, c.args])).toEqual([
      ["library_move", { id: 7, toFolder: "NOTE/Note/Home" }],
      ["library_rename", { id: 7, name: "b.pdf" }],
      ["library_new_folder", { parent: "NOTE/Note", name: "Plans" }],
      ["library_delete", { id: 7 }],
      ["library_download", { id: 7 }],
    ]);
    expect(Object.keys(library.jobs).map(Number)).toEqual([20, 21, 22, 23, 24]);
    expect(library.jobs[22].state).toBe("failed");
    expect(library.ended[20]).toBeDefined();

    answers.set("library_download", [() => Promise.reject("This is a folder, not a file.")]);
    await downloadEntry(8);
    expect(library.notice).toBe("This is a folder, not a file.");
  });
});

describe("file actions and the shown folder", () => {
  const changed = (id: number, state: Job["state"], result: string | null): Job => ({
    ...job,
    id,
    kind: "change",
    total: null,
    state,
    result,
  });

  it("follows a moved folder to the new path Rust answers", async () => {
    answerNow("library_move", changed(50, "done", "INBOX/Work"));
    const change = await moveRow(8, "NOTE/Note/Work", "INBOX");
    expect(calls.map((c) => [c.command, c.args])).toEqual([["library_move", { id: 8, toFolder: "INBOX" }]]);
    expect(change).toEqual({ from: "NOTE/Note/Work", to: "INBOX/Work" });
    expect(followChange({ shown: "NOTE/Note/Work/A", picked: null }, change!)).toEqual({ shown: "INBOX/Work/A", picked: null });
  });

  it("changes nothing shown when the job did not end done", async () => {
    answerNow("library_move", changed(51, "failed", null));
    expect(await moveRow(8, "NOTE/Note/Work", "INBOX")).toBeNull();
    answers.set("library_rename", [() => Promise.reject("The name is taken.")]);
    expect(await renameRow({ ...folder("NOTE/Note", ["a.pdf"], true).entries[0] }, "b.pdf")).toBeNull();
    expect(library.notice).toBe("The name is taken.");
  });

  it("renames only to a new name, and follows the rename", async () => {
    const entry = folder("NOTE/Note", ["a.pdf"], true).entries[0];
    expect(await renameRow(entry, null)).toBeNull();
    expect(await renameRow(entry, "  a.pdf ")).toBeNull();
    expect(calls).toEqual([]);
    answerNow("library_rename", changed(52, "done", "NOTE/Note/b.pdf"));
    expect(await renameRow(entry, " b.pdf ")).toEqual({ from: "NOTE/Note/a.pdf", to: "NOTE/Note/b.pdf" });
    expect(calls.map((c) => [c.command, c.args])).toEqual([["library_rename", { id: 1, name: "b.pdf" }]]);
  });

  it("makes a folder only with a name and a shown folder", async () => {
    await makeFolder(null, "Plans");
    await makeFolder("NOTE/Note", "  ");
    await makeFolder("NOTE/Note", null);
    expect(calls).toEqual([]);
    await makeFolder("NOTE/Note", " Plans ");
    expect(calls.map((c) => [c.command, c.args])).toEqual([["library_new_folder", { parent: "NOTE/Note", name: "Plans" }]]);
  });

  it("deletes an entry and reports it gone once done", async () => {
    const entry = folder("NOTE/Note", ["a.pdf"], true).entries[0];
    answerNow("library_delete", changed(53, "done", null));
    expect(await deleteRow(entry)).toEqual({ from: "NOTE/Note/a.pdf", to: null });
    answerNow("library_delete", changed(54, "failed", null));
    expect(await deleteRow(entry)).toBeNull();
  });
});

function binItem(path: string, deletedAt = 1_000): RecycleItem {
  return { key: `${deletedAt}/${path}`, path, name: path.slice(path.lastIndexOf("/") + 1), kind: "file", deletedAt, size: 10, files: null };
}

function page(items: RecycleItem[], next: string | null): RecyclePage {
  return { items, next };
}

function changeJob(id: number, state: Job["state"], rest: Partial<Job> = {}): Job {
  return { id, kind: "change", label: "Restore b.note", done: 0, total: null, state, text: null, question: null, result: null, dropId: null, ...rest };
}

const B = binItem("NOTE/Note/b.note");
const C = binItem("NOTE/Note/c.note", 500);

describe("the recycle bin", () => {
  it("lists page by page and from page 1 again, and drops an older answer", async () => {
    answerNow("library_recycle", page([B], "r1:b"));
    await listBin();
    expect(library.bin).toMatchObject({ items: [B], next: "r1:b", loaded: true, busy: false, error: null });
    expect(calls.at(-1)).toEqual({ command: "library_recycle", args: { cursor: null } });

    // The next page; one that overlaps shows each item once.
    answerNow("library_recycle", page([B, C], null));
    await listBinNext();
    expect(calls.at(-1)).toEqual({ command: "library_recycle", args: { cursor: "r1:b" } });
    expect(library.bin.items.map((i) => i.key)).toEqual([B.key, C.key]);
    expect(library.bin.next).toBeNull();
    // No next page: no call.
    await listBinNext();
    expect(calls.filter((c) => c.command === "library_recycle")).toHaveLength(2);

    // A next page that answers after page 1 was asked again is dropped.
    library.bin.next = "r1:b";
    const late = answerLater("library_recycle");
    const nextCall = listBinNext();
    const fresh = answerLater("library_recycle");
    const first = listBin();
    fresh.release(page([C], null));
    await first;
    late.release(page([B], null));
    await nextCall;
    expect(library.bin.items).toEqual([C]);

    // An error keeps what was shown and says why.
    const failing = answerLater("library_recycle");
    const failed = listBin();
    failing.fail("The server does not answer.");
    await failed;
    expect(library.bin).toMatchObject({ items: [C], error: "The server does not answer.", busy: false, loaded: true });

    // A new binding forgets the bin.
    applyEvent({ type: "relist" });
    expect(library.bin).toEqual({ items: [], next: null, loaded: false, busy: false, error: null });
  });

  it("restores an item, and asks for another name when the place is taken", async () => {
    answerNow("library_recycle", page([B, C], null));
    await listBin();

    // A plain restore: done, and the recycle event lists the bin again.
    answerNow("library_restore", changeJob(7, "done", { result: "NOTE/Note/c.note" }));
    await restoreItem(C);
    expect(calls.at(-1)).toEqual({ command: "library_restore", args: { key: C.key, path: null } });
    expect(library.restores).toEqual({});
    expect(library.jobs[7].state).toBe("done");

    // The place is taken: the row asks for another name.
    const clash = { name: "b.note", existing: "B.note", canReplace: false };
    answerNow("library_restore", changeJob(8, "asking", { text: "There is already B.note here.", question: clash }));
    await restoreItem(B);
    expect(library.restores[B.key]).toEqual({
      job: 8,
      state: "asking",
      text: "There is already B.note here.",
      name: "b.note",
      path: "NOTE/Note/b.note",
    });

    // The same name or no name is no answer: it keeps asking.
    await answerRestore(B.key, "b.note");
    await answerRestore(B.key, "  ");
    expect(calls.filter((c) => c.command === "library_restore")).toHaveLength(2);

    // A name that is not allowed asks again with the new text.
    answerNow(
      "library_restore",
      changeJob(8, "asking", { text: "This name cannot be used.", question: clash }),
    );
    await answerRestore(B.key, ".b.note");
    expect(calls.at(-1)).toEqual({ command: "library_restore", args: { key: B.key, path: "NOTE/Note/.b.note" } });
    expect(library.restores[B.key]).toMatchObject({ job: 8, state: "asking", text: "This name cannot be used." });

    // Another name goes on with the same job.
    const going = answerLater("library_restore");
    const answered = answerRestore(B.key, "b 2.note");
    expect(library.restores[B.key]).toMatchObject({ state: "busy" });
    going.release(changeJob(8, "done", { result: "NOTE/Note/b 2.note" }));
    await answered;
    expect(calls.at(-1)).toEqual({ command: "library_restore", args: { key: B.key, path: "NOTE/Note/b 2.note" } });
    expect(library.restores).toEqual({});
  });

  it("ends a restore that asks with Cancel, here or in the job rows", async () => {
    const clash = { name: "b.note", existing: "B.note", canReplace: false };
    answerNow("library_restore", changeJob(9, "asking", { text: "There is already B.note here.", question: clash }));
    await restoreItem(B);
    await cancelRestore(B.key);
    expect(calls.at(-1)).toEqual({ command: "library_cancel", args: { job: 9 } });
    expect(library.restores).toEqual({});

    // Cancelled from the job rows (or by a new binding): the job event ends the question.
    answerNow("library_restore", changeJob(10, "asking", { text: "There is already B.note here.", question: clash }));
    await restoreItem(B);
    applyEvent({ type: "job", job: changeJob(10, "cancelled") });
    expect(library.restores).toEqual({});
    // Escape in the name field is Cancel too.
    answerNow("library_restore", changeJob(11, "asking", { text: "There is already B.note here.", question: clash }));
    await restoreItem(B);
    await answerRestore(B.key, null);
    expect(calls.at(-1)).toEqual({ command: "library_cancel", args: { job: 11 } });
  });

  it("lists the bin again after a restore that failed; the error is the job's text", async () => {
    answerNow("library_recycle", page([B], null));
    await listBin();
    answerNow("library_restore", changeJob(12, "failed", { text: "This item is not in the recycle bin anymore." }));
    answerNow("library_recycle", page([], null));
    await restoreItem(B);
    expect(library.jobs[12].text).toBe("This item is not in the recycle bin anymore.");
    expect(library.restores).toEqual({});
    expect(calls.at(-1)).toEqual({ command: "library_recycle", args: { cursor: null } });
    expect(library.bin.items).toEqual([]);
  });

  it("empties the bin", async () => {
    answerNow("library_empty", changeJob(13, "done", { label: "Empty the recycle bin" }));
    await emptyBin();
    expect(calls.at(-1)).toEqual({ command: "library_empty", args: {} });
    expect(library.jobs[13].state).toBe("done");

    // A failed Empty lists the bin again: some items may be gone.
    answerNow("library_empty", changeJob(14, "failed", { text: "The server does not answer." }));
    answerNow("library_recycle", page([B], null));
    await emptyBin();
    expect(calls.at(-1)).toEqual({ command: "library_recycle", args: { cursor: null } });
  });
});
