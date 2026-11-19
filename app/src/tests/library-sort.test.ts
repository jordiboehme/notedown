import { describe, expect, it, vi } from "vitest";
import { render } from "svelte/server";
import NotesList from "../library/NotesList.svelte";
import RecycleBin from "../library/RecycleBin.svelte";
import FolderList from "../library/FolderList.svelte";
import type { NoteSummary, RecycleItem } from "../lib/library-types";
import { recycleRows } from "../library/recycle";
import {
  DEFAULT_SORTS,
  SORT_KEY,
  loadsRest,
  nextSort,
  readSort,
  saveSort,
  sortBin,
  sortNotes,
  type BinSort,
  type NotesSort,
} from "../library/sort";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(() => Promise.resolve(null)) }));

function note(title: string, rest: Partial<NoteSummary> = {}): NoteSummary {
  return {
    key: title,
    path: `NOTE/Note/${title}.note`,
    title,
    pages: 1,
    pagesWithText: 1,
    changedMs: 0,
    ...rest,
  };
}

function item(name: string, rest: Partial<RecycleItem> = {}): RecycleItem {
  return { key: name, path: `NOTE/Note/${name}`, name, kind: "file", deletedAt: 0, size: 10, files: null, ...rest };
}

/** A storage in memory; `broken` throws on every call, as a blocked one does. */
function memory(broken = false): Storage {
  const data = new Map<string, string>();
  const fail = () => {
    throw new Error("blocked");
  };
  return {
    getItem: (key: string) => (broken ? fail() : (data.get(key) ?? null)),
    setItem: (key: string, value: string) => (broken ? fail() : void data.set(key, value)),
    removeItem: (key: string) => void data.delete(key),
    clear: () => data.clear(),
    key: () => null,
    get length() {
      return data.size;
    },
  };
}

describe("a click on a column head", () => {
  it("sorts by it ascending, and a second click descending", () => {
    expect(nextSort({ by: "name", ascending: true }, "size")).toEqual({ by: "size", ascending: true });
    expect(nextSort({ by: "date", ascending: false }, "pages")).toEqual({ by: "pages", ascending: true });
    expect(nextSort({ by: "size", ascending: true }, "size")).toEqual({ by: "size", ascending: false });
    expect(nextSort({ by: "size", ascending: false }, "size")).toEqual({ by: "size", ascending: true });
  });
});

describe("the sort order of each list kind", () => {
  it("starts as the lists came before: newest notes and deletions first, names A to Z", () => {
    expect(DEFAULT_SORTS).toEqual({
      notes: { by: "date", ascending: false },
      folder: { by: "name", ascending: true },
      bin: { by: "deleted", ascending: false },
    });
    expect(readSort(memory(), "notes")).toEqual(DEFAULT_SORTS.notes);
    expect(readSort(null, "bin")).toEqual(DEFAULT_SORTS.bin);
  });

  it("is kept per list kind", () => {
    const storage = memory();
    saveSort(storage, "notes", { by: "pages", ascending: true });
    saveSort(storage, "bin", { by: "size", ascending: false });
    expect(readSort(storage, "notes")).toEqual({ by: "pages", ascending: true });
    expect(readSort(storage, "bin")).toEqual({ by: "size", ascending: false });
    expect(readSort(storage, "folder")).toEqual(DEFAULT_SORTS.folder);
    expect(storage.getItem(SORT_KEY.notes)).toBe('{"by":"pages","ascending":true}');
  });

  it("falls back to the default for anything it cannot read", () => {
    const storage = memory();
    storage.setItem(SORT_KEY.folder, "{nonsense");
    expect(readSort(storage, "folder")).toEqual(DEFAULT_SORTS.folder);
    // A column of another list kind is no column here.
    storage.setItem(SORT_KEY.folder, '{"by":"pages","ascending":true}');
    expect(readSort(storage, "folder")).toEqual(DEFAULT_SORTS.folder);
    storage.setItem(SORT_KEY.folder, '{"by":"size","ascending":"yes"}');
    expect(readSort(storage, "folder")).toEqual(DEFAULT_SORTS.folder);
    expect(readSort(memory(true), "folder")).toEqual(DEFAULT_SORTS.folder);
    expect(() => saveSort(memory(true), "folder", { by: "size", ascending: true })).not.toThrow();
  });
});

describe("the notes lists", () => {
  const notes = [
    note("Note 10", { pages: 2, changedMs: 3, path: "NOTE/Note/b/Note 10.note" }),
    note("note 2", { pages: 2, changedMs: 1, path: "NOTE/Note/a/note 2.note" }),
    note("Alpha", { pages: 5, changedMs: 2, path: "NOTE/Note/b/Alpha.note" }),
  ];
  const titles = (sort: NotesSort) => sortNotes(notes, sort).map((n) => n.title);

  it("sort names as the Finder does", () => {
    expect(titles({ by: "name", ascending: true })).toEqual(["Alpha", "note 2", "Note 10"]);
    expect(titles({ by: "name", ascending: false })).toEqual(["Note 10", "note 2", "Alpha"]);
  });

  it("sort by folder, pages and date, equal values by name", () => {
    expect(titles({ by: "folder", ascending: true })).toEqual(["note 2", "Alpha", "Note 10"]);
    expect(titles({ by: "pages", ascending: true })).toEqual(["note 2", "Note 10", "Alpha"]);
    expect(titles({ by: "pages", ascending: false })).toEqual(["Alpha", "note 2", "Note 10"]);
    expect(titles({ by: "date", ascending: false })).toEqual(["Note 10", "Alpha", "note 2"]);
  });

  it("keep the given list as it is", () => {
    sortNotes(notes, { by: "name", ascending: true });
    expect(notes.map((n) => n.title)).toEqual(["Note 10", "note 2", "Alpha"]);
  });

  it("show the sort in the column heads and the rows in its order", () => {
    const body = render(NotesList, {
      props: {
        title: "All notes",
        notes,
        empty: "No notes.",
        sort: { by: "pages", ascending: false },
        onSort: () => {},
        onOpen: () => {},
      },
    }).body;
    const heads = [...body.matchAll(/<button[^>]*class="head[^"]*"[^>]*>([^<]*)<span class="arrow[^"]*"[^>]*>([^<]*)<\/span>/g)].map(
      (m) => [m[1], m[2]],
    );
    expect(heads).toEqual([
      ["Name", ""],
      ["Folder", ""],
      ["Pages", "▾"],
      ["Changed", ""],
    ]);
    const rows = [...body.matchAll(/<span class="text(?! muted)[^"]*">([^<]*)<\/span>/g)].map((m) => m[1]);
    expect(rows).toEqual(["Alpha", "note 2", "Note 10"]);
  });
});

describe("the recycle bin", () => {
  const rows = recycleRows(
    [
      item("b.note", { deletedAt: 3, size: 50, path: "NOTE/Note/x/b.note" }),
      item("Work", { kind: "folder", deletedAt: 1, size: 0, files: 4, path: "NOTE/Note/Work" }),
      item("a.note", { deletedAt: 2, size: 5, path: "INBOX/a.note" }),
    ],
    10,
  );
  const names = (sort: BinSort) => sortBin(rows, sort).map((r) => r.name);

  it("sorts by each column", () => {
    expect(names({ by: "name", ascending: true })).toEqual(["a.note", "b.note", "Work"]);
    expect(names({ by: "folder", ascending: true })).toEqual(["a.note", "Work", "b.note"]);
    expect(names({ by: "deleted", ascending: false })).toEqual(["b.note", "a.note", "Work"]);
    expect(names({ by: "deleted", ascending: true })).toEqual(["Work", "a.note", "b.note"]);
    // A folder has no size of its own: it is the smallest.
    expect(names({ by: "size", ascending: true })).toEqual(["Work", "a.note", "b.note"]);
    expect(names({ by: "size", ascending: false })).toEqual(["b.note", "a.note", "Work"]);
  });

  it("shows the sort in its heads", () => {
    const body = render(RecycleBin, {
      props: {
        bin: { items: rows.map((r) => r.item), next: null, loaded: true, busy: false, error: null },
        rows,
        sort: { by: "name", ascending: true },
        onSort: () => {},
        restores: {},
        question: null,
        emptying: false,
        onMore: () => {},
        onRestore: () => {},
        onAnswer: () => {},
        onCancel: () => {},
        onDraft: () => {},
        onFocused: () => {},
        onAskEmpty: () => {},
        onDropEmpty: () => {},
        onEmpty: () => {},
      },
    }).body;
    const heads = [...body.matchAll(/<button[^>]*class="head[^"]*"[^>]*>([^<]*)<span class="arrow[^"]*"[^>]*>([^<]*)<\/span>/g)].map(
      (m) => [m[1], m[2]],
    );
    expect(heads).toEqual([
      ["Name", "▴"],
      ["Was in", ""],
      ["Deleted", ""],
      ["Size", ""],
    ]);
  });
});

describe("a list that comes in pages", () => {
  it("loads the rest when its sort is not the one it comes in, or a head was clicked", () => {
    expect(loadsRest("folder", DEFAULT_SORTS.folder, false)).toBe(false);
    expect(loadsRest("folder", { by: "name", ascending: false }, false)).toBe(true);
    expect(loadsRest("folder", DEFAULT_SORTS.folder, true)).toBe(true);
    expect(loadsRest("bin", DEFAULT_SORTS.bin, false)).toBe(false);
    expect(loadsRest("bin", { by: "size", ascending: true }, false)).toBe(true);
  });

  it("can be sorted while it loads", () => {
    const body = render(FolderList, {
      props: {
        path: "INBOX",
        folder: { path: "INBOX", entries: [], complete: false, loading: true, error: null },
        next: "2",
        busy: true,
        sort: { by: "name", ascending: true },
        picked: null,
        renaming: null,
        onSort: () => {},
        onOpen: () => {},
        onOpenNote: () => {},
        onMore: () => {},
        onPick: () => {},
        onRename: () => {},
      },
    }).body;
    const heads = [...body.matchAll(/<button[^>]*class="head[^"]*"[^>]*>/g)].map((m) => m[0]);
    expect(heads).toHaveLength(3);
    for (const head of heads) expect(head).not.toContain("disabled");
  });
});
