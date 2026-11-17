import { describe, expect, it } from "vitest";
import type { RecycleItem } from "../lib/library-types";
import { binNeedsNext, binView, emptyQuestion, mergeBin, recycleRows, restoreTarget } from "../library/recycle";

const NOW = Date.UTC(2026, 10, 17, 12, 0, 0);

function item(path: string, kind: "file" | "folder", rest: Partial<RecycleItem> = {}): RecycleItem {
  const deletedAt = rest.deletedAt ?? NOW - 5 * 60_000;
  return {
    key: `${deletedAt}/${path}`,
    path,
    name: path.slice(path.lastIndexOf("/") + 1),
    kind,
    deletedAt,
    size: 20_480,
    files: kind === "folder" ? 4 : null,
    ...rest,
  };
}

describe("the rows of the recycle bin", () => {
  it("say the name, the folder it was in, when it was deleted and its size", () => {
    const rows = recycleRows(
      [
        item("NOTE/Note/Work/b.note", "file"),
        item("NOTE/Note/projects", "folder", { deletedAt: NOW - 3 * 3_600_000 }),
        item("INBOX/one", "folder", { files: 1 }),
        item("INBOX/old", "folder", { files: null }),
        item("EXPORT/1234.pdf", "file", { files: null, size: 2_000_000 }),
      ],
      NOW,
    );
    expect(rows.map((r) => [r.name, r.folder, r.deleted, r.size])).toEqual([
      ["b.note", "Note / Work", "5 min ago", "20 KB"],
      ["projects", "Note", "3 h ago", "4 files"],
      ["one", "Inbox", "5 min ago", "1 file"],
      ["old", "Inbox", "5 min ago", ""],
      ["1234.pdf", "Export", "5 min ago", "2.0 MB"],
    ]);
    expect(rows[1].kind).toBe("folder");
    expect(rows[0].key).toBe(`${NOW - 5 * 60_000}/NOTE/Note/Work/b.note`);
  });

  it("count many files with a thousands separator", () => {
    expect(recycleRows([item("INBOX/big", "folder", { files: 12_345 })], NOW)[0].size).toBe("12,345 files");
  });

  it("show an item only once when pages overlap", () => {
    const a = item("INBOX/a.pdf", "file");
    const b = item("INBOX/b.pdf", "file", { deletedAt: NOW - 60 * 60_000 });
    const c = item("INBOX/c.pdf", "file", { deletedAt: NOW - 2 * 60 * 60_000 });
    // The next page starts after the last item of the page; a row there twice comes once.
    const items = mergeBin(mergeBin([], [a, b]), [b, c]);
    expect(items.map((i) => i.name)).toEqual(["a.pdf", "b.pdf", "c.pdf"]);
    expect(recycleRows([a, a, b], NOW).map((r) => r.name)).toEqual(["a.pdf", "b.pdf"]);
  });
});

describe("the recycle bin view", () => {
  it("says the bin is empty only once it is known", () => {
    expect(binView({ items: [], next: null, loaded: false, busy: true, error: null })).toEqual({
      loading: true,
      empty: false,
      canEmpty: false,
    });
    expect(binView({ items: [], next: null, loaded: true, busy: false, error: null })).toEqual({
      loading: false,
      empty: true,
      canEmpty: false,
    });
    // A first page without items but with a next page is not an empty bin.
    expect(binView({ items: [], next: "r1:x", loaded: true, busy: false, error: null })).toMatchObject({
      empty: false,
      canEmpty: true,
    });
    expect(binView({ items: [item("INBOX/a", "file")], next: null, loaded: true, busy: false, error: null })).toEqual({
      loading: false,
      empty: false,
      canEmpty: true,
    });
    // A bin that could not be read is not called empty.
    expect(binView({ items: [], next: null, loaded: true, busy: false, error: "The server does not answer." })).toMatchObject({
      empty: false,
      canEmpty: false,
    });
  });

  it("asks before Empty, with the count when every page is loaded", () => {
    expect(emptyQuestion({ items: [item("INBOX/a", "file")], next: null })).toBe("Delete 1 item for good? This cannot be undone.");
    expect(emptyQuestion({ items: [item("INBOX/a", "file"), item("INBOX/b", "file")], next: null })).toBe(
      "Delete 2 items for good? This cannot be undone.",
    );
    expect(emptyQuestion({ items: [item("INBOX/a", "file")], next: "r1:x" })).toBe(
      "Delete all items for good? This cannot be undone.",
    );
  });

  it("restores under a new name in the folder the item was in", () => {
    expect(restoreTarget("NOTE/Note/Work/b.note", " c.note ", "b.note")).toEqual({ path: "NOTE/Note/Work/c.note" });
    expect(restoreTarget("INBOX/a.pdf", "a 2.pdf", "a.pdf")).toEqual({ path: "INBOX/a 2.pdf" });
    expect(restoreTarget("a.pdf", "b.pdf", "a.pdf")).toEqual({ path: "b.pdf" });
    // An empty name, or the name that is taken, is no answer: the row says so.
    expect(restoreTarget("INBOX/a.pdf", "  ", "a.pdf")).toEqual({ refuse: "Type another name." });
    expect(restoreTarget("INBOX/a.pdf", "a.pdf", "a.pdf")).toEqual({ refuse: "Type another name." });
    // A name with a slash would put the item in another (new) folder.
    const own = { refuse: "Restore keeps the item in its folder. Type a name without /." };
    expect(restoreTarget("NOTE/Note/Work/b.note", "sub/b.note", "b.note")).toEqual(own);
    expect(restoreTarget("NOTE/Note/Work/b.note", "/b.note", "b.note")).toEqual(own);
  });

  it("asks for the next page only while the bin can be read", () => {
    const bin = { items: [], next: "r1:x", loaded: true, busy: false, error: null };
    expect(binNeedsNext(bin, 0, 0)).toBe(true);
    expect(binNeedsNext({ ...bin, busy: true }, 0, 0)).toBe(false);
    expect(binNeedsNext({ ...bin, next: null }, 0, 0)).toBe(false);
    // A next page that failed is not asked for again at once (no loop without a wait).
    expect(binNeedsNext({ ...bin, error: "The server does not answer." }, 0, 0)).toBe(false);
  });
});
