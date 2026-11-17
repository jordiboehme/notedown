import { describe, expect, it } from "vitest";
import type { Entry, FolderState, LibraryState, LibraryStateView } from "../lib/library-types";
import {
  ROOTS,
  columnsView,
  folderLabel,
  listRows,
  listingKey,
  needsNextPage,
  parentOf,
  pathBar,
  rootRows,
  shownFor,
  shownListings,
  viewButtons,
  sidebarItems,
  stateView,
  treeView,
  visibleRange,
} from "../library/view";

let nextId = 1;

function entry(path: string, kind: "file" | "folder", rest: Partial<Entry> = {}): Entry {
  return {
    id: nextId++,
    path,
    name: path.slice(path.lastIndexOf("/") + 1),
    kind,
    size: kind === "file" ? 100 : null,
    md5: kind === "file" ? "0cc175b9c0f1b6a831c399e269772661" : null,
    mtimeMs: 1_000,
    writable: true,
    ...rest,
  };
}

function folder(path: string, entries: Entry[], complete = true): FolderState {
  return { path, entries, complete, loading: false, error: null };
}

const ready: LibraryStateView = { state: "ready", text: null, page: null };

describe("the sidebar", () => {
  it("has New with its count, All notes, the device trees and the recycle bin when ready", () => {
    const items = sidebarItems(ready, { newNotes: 3, allNotes: 1_214 }, { kind: "new" });
    expect(items.notes).toEqual([
      { id: "new", label: "New", count: "3", selected: true },
      { id: "all", label: "All notes", count: "1,214", selected: false },
    ]);
    expect(items.tree).toBe(true);
    expect(items.bin).toEqual({ id: "bin", label: "Recycle bin", count: null, selected: false });
  });

  it("shows no count before the notes are read, and none for no new notes", () => {
    const items = sidebarItems(ready, { newNotes: null, allNotes: null }, { kind: "bin" });
    expect(items.notes.map((i) => i.count)).toEqual([null, null]);
    expect(items.bin?.selected).toBe(true);
    expect(sidebarItems(ready, { newNotes: 0, allNotes: 4 }, { kind: "all" }).notes[0].count).toBeNull();
  });

  it("keeps the notes but not the library while the library cannot be read", () => {
    const items = sidebarItems(
      { state: "loggedOut", text: "Log in to see your library.", page: "account" },
      { newNotes: 1, allNotes: 2 },
      { kind: "new" },
    );
    expect(items.notes).toHaveLength(2);
    expect(items.tree).toBe(false);
    expect(items.bin).toBeNull();
    expect(sidebarItems(null, { newNotes: 1, allNotes: 2 }, { kind: "new" }).tree).toBe(false);
  });
});

describe("the tree", () => {
  it("has the six device trees, closed, with nothing to list", () => {
    const view = treeView({}, new Set(), null);
    expect(view.rows.map((r) => [r.name, r.path, r.depth, r.open])).toEqual(
      ROOTS.map((root) => [root.name, root.path, 0, false]),
    );
    expect(ROOTS.map((r) => r.path)).toEqual([
      "NOTE/Note",
      "DOCUMENT/Document",
      "INBOX",
      "EXPORT",
      "SCREENSHOT",
      "NOTE/MyStyle",
    ]);
    expect(view.missing).toEqual([]);
  });

  it("lists a folder only when it is opened, and shows that it loads", () => {
    const view = treeView({}, new Set(["NOTE/Note"]), null);
    expect(view.missing).toEqual(["NOTE/Note"]);
    const note = view.rows.find((r) => r.path === "NOTE/Note")!;
    expect(note.open).toBe(true);
    expect(note.loading).toBe(true);
    // Nothing below a closed folder is listed, even when its parent is open.
    const listings = {
      [listingKey("NOTE/Note", true)]: folder("NOTE/Note", [entry("NOTE/Note/Work", "folder")]),
    };
    const opened = treeView(listings, new Set(["NOTE/Note"]), null);
    expect(opened.missing).toEqual([]);
    expect(opened.rows.map((r) => r.path).slice(0, 3)).toEqual(["NOTE/Note", "NOTE/Note/Work", "DOCUMENT/Document"]);
    expect(opened.rows[1]).toMatchObject({ depth: 1, open: false, loading: false });
  });

  it("shows folders only, in natural order, and marks the selected one", () => {
    const listings = {
      // Rust sends only folders for the tree; a file would still not show.
      [listingKey("INBOX", true)]: folder("INBOX", [
        entry("INBOX/b.pdf", "file"),
        entry("INBOX/Box 10", "folder"),
        entry("INBOX/box 9", "folder"),
      ]),
    };
    const view = treeView(listings, new Set(["INBOX"]), "INBOX/Box 10");
    expect(view.missing).toEqual([]);
    const below = view.rows.filter((r) => r.depth === 1);
    expect(below.map((r) => r.name)).toEqual(["box 9", "Box 10"]);
    expect(below.find((r) => r.name === "Box 10")!.selected).toBe(true);
  });

  it("takes only its own listing and says when an open folder has no folders or failed", () => {
    const listings = {
      [listingKey("EXPORT", true)]: folder("EXPORT", []),
      [listingKey("EXPORT", false)]: folder("EXPORT", [entry("EXPORT/old", "folder")]),
      [listingKey("INBOX", true)]: { ...folder("INBOX", [], false), error: "This folder is not there anymore." },
    };
    const view = treeView(listings, new Set(["EXPORT", "INBOX"]), null);
    // The list's listing of a path is followed for the list: the tree lists the path itself.
    expect(treeView({ [listingKey("EXPORT", false)]: folder("EXPORT", []) }, new Set(["EXPORT"]), null).missing).toEqual([
      "EXPORT",
    ]);
    const exportRow = view.rows.find((r) => r.path === "EXPORT")!;
    expect(exportRow.empty).toBe(true);
    expect(view.rows.some((r) => r.path === "EXPORT/old")).toBe(false);
    expect(view.rows.find((r) => r.path === "INBOX")!.error).toBe("This folder is not there anymore.");
  });

  it("goes on listing an open folder that has more pages", () => {
    const listings = {
      [listingKey("NOTE/Note", true)]: folder("NOTE/Note", [entry("NOTE/Note/a", "folder")], false),
    };
    const view = treeView(listings, new Set(["NOTE/Note"]), null);
    expect(view.more).toEqual(["NOTE/Note"]);
    expect(view.rows.find((r) => r.path === "NOTE/Note")!.loading).toBe(true);
  });
});

describe("the list", () => {
  const files = [
    entry("INBOX/file 10.pdf", "file", { size: 300, mtimeMs: 3 }),
    entry("INBOX/Zeta", "folder", { mtimeMs: 9 }),
    entry("INBOX/File 9.pdf", "file", { size: 100, mtimeMs: 1 }),
    entry("INBOX/alpha", "folder", { mtimeMs: 2 }),
    entry("INBOX/b.epub", "file", { size: 200, mtimeMs: 7 }),
  ];
  const names = (rows: Entry[]) => rows.map((r) => r.name);

  it("keeps the server order while the folder loads, whatever the sort", () => {
    const loading = folder("INBOX", files, false);
    for (const by of ["name", "size", "date"] as const) {
      const view = listRows(loading, { by, ascending: true });
      expect(names(view.rows)).toEqual(names(files));
      expect(view.sortable).toBe(false);
    }
  });

  it("puts folders first and names in natural, case-insensitive order when complete", () => {
    const view = listRows(folder("INBOX", files), { by: "name", ascending: true });
    expect(view.sortable).toBe(true);
    expect(names(view.rows)).toEqual(["alpha", "Zeta", "b.epub", "File 9.pdf", "file 10.pdf"]);
    const down = listRows(folder("INBOX", files), { by: "name", ascending: false });
    expect(names(down.rows)).toEqual(["Zeta", "alpha", "file 10.pdf", "File 9.pdf", "b.epub"]);
  });

  it("sorts by size and by date only when complete", () => {
    const complete = folder("INBOX", files);
    expect(names(listRows(complete, { by: "size", ascending: true }).rows)).toEqual([
      "alpha",
      "Zeta",
      "File 9.pdf",
      "b.epub",
      "file 10.pdf",
    ]);
    expect(names(listRows(complete, { by: "date", ascending: false }).rows)).toEqual([
      "Zeta",
      "alpha",
      "b.epub",
      "file 10.pdf",
      "File 9.pdf",
    ]);
  });

  it("a_folder_loads_its_next_page_near_the_end", () => {
    const shown = visibleRange({ scrollTop: 0, height: 600, rowHeight: 24, count: 200 });
    expect(needsNextPage({ end: shown.end, count: 200, next: "t1:x", busy: false })).toBe(false);
    const nearEnd = visibleRange({ scrollTop: 24 * 150, height: 600, rowHeight: 24, count: 200 });
    expect(needsNextPage({ end: nearEnd.end, count: 200, next: "t1:x", busy: false })).toBe(true);
    // Not twice at once, and not past the last page.
    expect(needsNextPage({ end: nearEnd.end, count: 200, next: "t1:x", busy: true })).toBe(false);
    expect(needsNextPage({ end: nearEnd.end, count: 200, next: null, busy: false })).toBe(false);
    // A first page that fills less than the view loads the next one at once.
    const short = visibleRange({ scrollTop: 0, height: 600, rowHeight: 24, count: 10 });
    expect(needsNextPage({ end: short.end, count: 10, next: "t1:x", busy: false })).toBe(true);
  });

  it("only_visible_rows_are_rendered", () => {
    const range = visibleRange({ scrollTop: 24 * 1_000, height: 240, rowHeight: 24, count: 10_000, overscan: 5 });
    expect(range.start).toBe(995);
    expect(range.end).toBe(1_015);
    expect(range.end - range.start).toBeLessThan(30);
    // The spacers keep the full height, so the scroll bar is right.
    expect(range.top).toBe(995 * 24);
    expect(range.top + (range.end - range.start) * 24 + range.bottom).toBe(10_000 * 24);
    const end = visibleRange({ scrollTop: 24 * 9_999, height: 240, rowHeight: 24, count: 10_000, overscan: 5 });
    expect(end.end).toBe(10_000);
    expect(end.bottom).toBe(0);
    expect(visibleRange({ scrollTop: 0, height: 240, rowHeight: 24, count: 0 })).toEqual({
      start: 0,
      end: 0,
      top: 0,
      bottom: 0,
    });
  });
});

describe("the path bar", () => {
  it("names the device tree as one part and each folder below it", () => {
    expect(pathBar("DOCUMENT/Document/Books/Sci-Fi")).toEqual([
      { label: "Document", path: "DOCUMENT/Document" },
      { label: "Books", path: "DOCUMENT/Document/Books" },
      { label: "Sci-Fi", path: "DOCUMENT/Document/Books/Sci-Fi" },
    ]);
    expect(pathBar("NOTE/MyStyle")).toEqual([{ label: "MyStyle", path: "NOTE/MyStyle" }]);
    expect(pathBar("INBOX/a")).toEqual([
      { label: "Inbox", path: "INBOX" },
      { label: "a", path: "INBOX/a" },
    ]);
  });

  it("names the folder of a note", () => {
    expect(folderLabel("NOTE/Note/Work/Meeting.note")).toBe("Note / Work");
    expect(folderLabel("INBOX/a.pdf")).toBe("Inbox");
  });

  it("takes a path outside the trees part by part", () => {
    expect(pathBar("NOTE/Other")).toEqual([
      { label: "NOTE", path: "NOTE" },
      { label: "Other", path: "NOTE/Other" },
    ]);
  });
});

describe("the shown folders", () => {
  it("are the selected folder and the open folders of the tree", () => {
    expect(shownListings({ kind: "folder", path: "INBOX/a" }, new Set(["NOTE/Note", "INBOX"]))).toEqual([
      { path: "INBOX", foldersOnly: true },
      { path: "INBOX/a", foldersOnly: false },
      { path: "NOTE/Note", foldersOnly: true },
    ]);
    // While the notes or the bin are shown, the tree still is.
    expect(shownListings({ kind: "bin" }, new Set(["EXPORT"]))).toEqual([{ path: "EXPORT", foldersOnly: true }]);
  });
});

describe("the state of the library", () => {
  const states: [LibraryState, string, string][] = [
    ["notSetUp", "Choose where the server runs in Settings.", "server"],
    ["loggedOut", "Log in to see your library.", "account"],
    ["unreachable", "The server does not answer.", "server"],
    ["notImported", "The Supernote has not been imported yet.", "supernote"],
    ["refused", "The server refused the login. Log in again.", "account"],
  ];

  it("shows each state with its text and a Settings button", () => {
    for (const [state, text, page] of states) {
      expect(stateView({ state, text, page })).toEqual({ text, page, action: "Settings" });
      // Without a text from Rust, the state still says what is wrong.
      expect(stateView({ state, text: null, page })?.text).toBe(text);
    }
  });

  it("shows nothing when ready, and waits before the first binding", () => {
    expect(stateView(ready)).toBeNull();
    expect(stateView(null)).toEqual({ text: "Looking at the library...", page: null, action: null });
  });
});

describe("the state notice", () => {
  it("shows the text of each state and its Settings button", async () => {
    const { render } = await import("svelte/server");
    const { default: StateNotice } = await import("../library/StateNotice.svelte");
    const states: LibraryState[] = ["notSetUp", "loggedOut", "unreachable", "notImported", "refused"];
    for (const state of states) {
      const body = render(StateNotice, {
        props: { view: { state, text: `Text of ${state}.`, page: "server" }, onSettings: () => {} },
      }).body;
      expect(body).toContain(`Text of ${state}.`);
      expect(body).toContain(">Settings</button>");
    }
    const waiting = render(StateNotice, { props: { view: null, onSettings: () => {} } }).body;
    expect(waiting).toContain("Looking at the library...");
    expect(waiting).not.toContain("Settings</button>");
  });

  it("keeps note text and names as text", async () => {
    const { render } = await import("svelte/server");
    const { default: StateNotice } = await import("../library/StateNotice.svelte");
    const body = render(StateNotice, {
      props: { view: { state: "unreachable", text: "<img src=x onerror=alert(1)>", page: null }, onSettings: () => {} },
    }).body;
    expect(body).not.toContain("<img");
    expect(body).toContain("&lt;img");
  });
});

describe("the columns", () => {
  const path = "DOCUMENT/Document/Books/Sci-Fi";
  const document = folder("DOCUMENT/Document", [
    entry("DOCUMENT/Document/Papers", "folder"),
    entry("DOCUMENT/Document/Books", "folder"),
    entry("DOCUMENT/Document/Manual.pdf", "file"),
  ]);
  const books = folder("DOCUMENT/Document/Books", [
    entry("DOCUMENT/Document/Books/Tech", "folder"),
    entry("DOCUMENT/Document/Books/Sci-Fi", "folder"),
  ]);
  const scifi = folder(path, [
    entry(`${path}/Neuromancer.epub`, "file"),
    entry(`${path}/Classics`, "folder"),
    entry(`${path}/Dune.pdf`, "file"),
  ]);

  it("has the device trees as the first column and one column per level of the path", () => {
    const view = columnsView(path, {
      [listingKey("DOCUMENT/Document", false)]: document,
      [listingKey("DOCUMENT/Document/Books", false)]: books,
      [listingKey(path, false)]: scifi,
    }, `${path}/Dune.pdf`);
    expect(view.columns.map((c) => c.path)).toEqual([null, "DOCUMENT/Document", "DOCUMENT/Document/Books", path]);
    expect(view.columns[0].rows.map((r) => [r.name, r.selected])).toEqual(
      ROOTS.map((r) => [r.name, r.path === "DOCUMENT/Document"]),
    );
    // Every column lists folders then files, with the next level selected.
    expect(view.columns[1].rows.map((r) => [r.name, r.selected])).toEqual([
      ["Books", true],
      ["Papers", false],
      ["Manual.pdf", false],
    ]);
    expect(view.columns[2].rows.map((r) => [r.name, r.selected])).toEqual([
      ["Sci-Fi", true],
      ["Tech", false],
    ]);
    // The last column is the folder itself; a picked file is selected, no column follows it.
    expect(view.columns[3].rows.map((r) => [r.name, r.kind, r.selected])).toEqual([
      ["Classics", "folder", false],
      ["Dune.pdf", "file", true],
      ["Neuromancer.epub", "file", false],
    ]);
    expect(view.columns.map((c) => c.last)).toEqual([false, false, false, true]);
  });

  it("lists a folder only when it is opened: every column of the path, files too", () => {
    const view = columnsView(path, {});
    expect(view.shown).toEqual([
      { path: "DOCUMENT/Document", foldersOnly: false },
      { path: "DOCUMENT/Document/Books", foldersOnly: false },
      { path: path, foldersOnly: false },
    ]);
    expect(view.missing).toEqual(view.shown);
    expect(view.columns.slice(1).every((c) => c.loading)).toBe(true);
    expect(columnsView(null, {}).shown).toEqual([]);
    expect(columnsView(null, {}).columns).toHaveLength(1);
  });

  it("lists the columns without a listing as missing, the ones listed are not", () => {
    const view = columnsView(path, { [listingKey("DOCUMENT/Document", false)]: document });
    expect(view.missing.map((m) => m.path)).toEqual(["DOCUMENT/Document/Books", path]);
  });

  it("pages lazily: a column that has more pages loads, a failed one shows its error", () => {
    const part = folder("DOCUMENT/Document", [entry("DOCUMENT/Document/Books", "folder")], false);
    const failed: FolderState = { ...folder(path, []), complete: false, error: "The server does not answer." };
    const view = columnsView(path, {
      [listingKey("DOCUMENT/Document", false)]: part,
      [listingKey(path, false)]: failed,
    });
    expect(view.columns[1].loading).toBe(true);
    expect(view.columns[3].loading).toBe(false);
    expect(view.columns[3].error).toBe("The server does not answer.");
    expect(view.missing.map((m) => m.path)).toEqual(["DOCUMENT/Document/Books"]);
  });

  it("marks an empty complete folder", () => {
    const view = columnsView("INBOX", { [listingKey("INBOX", false)]: folder("INBOX", []) });
    expect(view.columns.map((c) => c.path)).toEqual([null, "INBOX"]);
    expect(view.columns[1].empty).toBe(true);
    expect(view.columns[1].last).toBe(true);
  });

  it("closes the columns right of a file picked in a middle column", () => {
    const file = "DOCUMENT/Document/Manual.pdf";
    expect(parentOf(file)).toBe("DOCUMENT/Document");
    const view = columnsView(parentOf(file), { [listingKey("DOCUMENT/Document", false)]: document }, file);
    expect(view.columns.map((c) => c.path)).toEqual([null, "DOCUMENT/Document"]);
    expect(view.columns[1].rows.filter((r) => r.selected).map((r) => r.name)).toEqual(["Manual.pdf"]);
  });

  it("keeps a root as one level", () => {
    const view = columnsView("NOTE/Note", {});
    expect(view.columns.map((c) => c.path)).toEqual([null, "NOTE/Note"]);
    expect(view.columns[0].rows.find((r) => r.selected)?.name).toBe("Note");
  });
});

describe("the view button", () => {
  it("shows Tree and Columns with the shown one pressed", () => {
    expect(viewButtons("tree")).toEqual([
      { id: "tree", label: "Tree", pressed: true },
      { id: "columns", label: "Columns", pressed: false },
    ]);
    expect(viewButtons("columns").map((b) => b.pressed)).toEqual([false, true]);
  });

  it("shows the open tree folders in the tree and the columns of the path in the columns", () => {
    const open = new Set(["INBOX"]);
    const sel = { kind: "folder", path: "NOTE/Note/Work" } as const;
    expect(shownFor("tree", sel, open)).toEqual(shownListings(sel, open));
    expect(shownFor("columns", sel, open)).toEqual([
      { path: "NOTE/Note", foldersOnly: false },
      { path: "NOTE/Note/Work", foldersOnly: false },
    ]);
    expect(shownFor("columns", { kind: "all" }, open)).toEqual([]);
  });

  it("lists the device trees as flat rows in the sidebar of the columns", () => {
    const rows = rootRows("INBOX");
    expect(rows.map((r) => [r.name, r.depth, r.open, r.empty, r.selected])).toEqual(
      ROOTS.map((r) => [r.name, 0, false, true, r.path === "INBOX"]),
    );
  });
});
