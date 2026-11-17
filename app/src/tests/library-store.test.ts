import { beforeEach, describe, expect, it, vi } from "vitest";
import type { FolderState, Job, LibraryEvent, Listing } from "../lib/library-types";

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

