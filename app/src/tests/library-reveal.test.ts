import { describe, expect, it } from "vitest";
import {
  afterBack,
  columnsPlace,
  columnsView,
  crumbTarget,
  noteOrigin,
  revealOf,
  rootRows,
  shownFor,
  treeSelected,
  treeView,
  type BackTo,
  type Reveal,
  type Selection,
} from "../library/view";

describe("the reveal of an open note", () => {
  it("is the note's folder, opened with the folders above it", () => {
    expect(revealOf("NOTE/Note/Work/2026/Plan.note")).toEqual({
      note: "NOTE/Note/Work/2026/Plan.note",
      folder: "NOTE/Note/Work/2026",
      opens: ["NOTE/Note", "NOTE/Note/Work", "NOTE/Note/Work/2026"],
    });
  });

  it("is the device tree itself for a note right in it", () => {
    expect(revealOf("NOTE/Note/Plan.note")).toEqual({ note: "NOTE/Note/Plan.note", folder: "NOTE/Note", opens: ["NOTE/Note"] });
    expect(revealOf("DOCUMENT/Document/a/b.note")).toEqual({
      note: "DOCUMENT/Document/a/b.note",
      folder: "DOCUMENT/Document/a",
      opens: ["DOCUMENT/Document", "DOCUMENT/Document/a"],
    });
  });

  it("is nothing for a note outside the device trees", () => {
    expect(revealOf("NOTE/Plan.note")).toBeNull();
    expect(revealOf("Other/Plan.note")).toBeNull();
    expect(revealOf("Plan.note")).toBeNull();
  });
});

describe("the folder the sidebar selects", () => {
  const reveal: Reveal = { note: "NOTE/Note/Work/Plan.note", folder: "NOTE/Note/Work", opens: ["NOTE/Note"] };
  const note: Selection = { kind: "note", path: "NOTE/Note/Work/Plan.note" };

  it("is the shown folder", () => {
    expect(treeSelected({ kind: "folder", path: "NOTE/Note/A" }, reveal)).toBe("NOTE/Note/A");
  });

  it("is the open note's folder once the note opened", () => {
    expect(treeSelected(note, reveal)).toBe("NOTE/Note/Work");
    // Before it opened, or for another note (gone, in the bin): nothing changes.
    expect(treeSelected(note, null)).toBeNull();
    expect(treeSelected({ kind: "note", path: "NOTE/Note/Other.note" }, reveal)).toBeNull();
  });

  it("is nothing for New, All notes and the bin", () => {
    for (const kind of ["new", "all", "bin"] as const) expect(treeSelected({ kind }, reveal)).toBeNull();
  });

  it("marks the row in the tree and the device tree in the columns' sidebar", () => {
    const open = new Set(["NOTE/Note"]);
    const listings = {
      "tree:NOTE/Note": {
        path: "NOTE/Note",
        complete: true,
        loading: false,
        error: null,
        entries: [
          {
            id: 1,
            path: "NOTE/Note/Work",
            name: "Work",
            kind: "folder" as const,
            size: null,
            md5: null,
            mtimeMs: 0,
            writable: true,
          },
        ],
      },
    };
    const rows = treeView(listings, open, treeSelected(note, reveal)).rows;
    expect(rows.filter((r) => r.selected).map((r) => r.path)).toEqual(["NOTE/Note/Work"]);
    expect(rootRows(treeSelected(note, reveal)).filter((r) => r.selected).map((r) => r.path)).toEqual(["NOTE/Note"]);
  });
});

describe("Back from a note", () => {
  it("goes where the note was opened from, whatever the reveal did", () => {
    // The sidebar showed the note's folder; Back still goes to New, All notes or the bin.
    for (const kind of ["new", "all", "bin"] as const) {
      expect(afterBack({ selection: { kind }, picked: null, search: null })).toEqual({
        selection: { kind },
        picked: null,
        search: null,
      });
    }
  });

  it("gives a folder back its picked row", () => {
    expect(afterBack({ selection: { kind: "folder", path: "INBOX" }, picked: "INBOX/Plan.note", search: null })).toEqual({
      selection: { kind: "folder", path: "INBOX" },
      picked: "INBOX/Plan.note",
      search: null,
    });
  });

  it("gives the search results back when the note came from them", () => {
    expect(afterBack({ selection: { kind: "all" }, picked: null, search: "plan" })).toEqual({
      selection: { kind: "all" },
      picked: null,
      search: "plan",
    });
    // An empty search is no search to give back.
    expect(afterBack({ selection: { kind: "all" }, picked: null, search: "  " }).search).toBeNull();
  });
});

describe("the columns while a note is open", () => {
  const note: Selection = { kind: "note", path: "NOTE/Note/Work/Plan.note" };
  const reveal = revealOf("NOTE/Note/Work/Plan.note");

  it("show the path to the note's folder with the note picked", () => {
    expect(columnsPlace(note, reveal, null)).toEqual({ path: "NOTE/Note/Work", file: "NOTE/Note/Work/Plan.note" });
    const shown = columnsView("NOTE/Note/Work", {}, "NOTE/Note/Work/Plan.note");
    expect(shown.columns.map((c) => c.path)).toEqual([null, "NOTE/Note", "NOTE/Note/Work"]);
    // Rust follows those folders while the note is open, so the columns are ready.
    expect(shownFor("columns", note, new Set(), reveal)).toEqual(shown.shown);
  });

  it("show the shown folder with its picked file, and nothing before the reveal", () => {
    expect(columnsPlace({ kind: "folder", path: "INBOX" }, null, "INBOX/a.pdf")).toEqual({ path: "INBOX", file: "INBOX/a.pdf" });
    expect(columnsPlace(note, null, null)).toEqual({ path: null, file: null });
    expect(columnsPlace({ kind: "new" }, reveal, null)).toEqual({ path: null, file: null });
    expect(shownFor("columns", note, new Set(), null)).toEqual([]);
  });

  it("do not change what the tree follows", () => {
    expect(shownFor("tree", note, new Set(["NOTE/Note"]), reveal)).toEqual([{ path: "NOTE/Note", foldersOnly: true }]);
  });
});

describe("a click on a folder of the path", () => {
  const note: Selection = { kind: "note", path: "NOTE/Note/Work/Plan.note" };

  it("on the open note's folder shows it with the note picked", () => {
    expect(crumbTarget(note, "NOTE/Note/Work")).toEqual({ folder: "NOTE/Note/Work", picked: "NOTE/Note/Work/Plan.note" });
  });

  it("on any other folder shows it with nothing picked", () => {
    expect(crumbTarget(note, "NOTE/Note")).toEqual({ folder: "NOTE/Note", picked: null });
    expect(crumbTarget({ kind: "folder", path: "NOTE/Note/Work" }, "NOTE/Note/Work")).toEqual({
      folder: "NOTE/Note/Work",
      picked: null,
    });
  });
});

describe("where a note is opened from", () => {
  const none: BackTo = { selection: { kind: "new" }, picked: null, search: null };

  it("is the shown place with its picked row", () => {
    expect(noteOrigin(none, { selection: { kind: "folder", path: "INBOX" }, picked: "INBOX/a.note" }, null)).toEqual({
      selection: { kind: "folder", path: "INBOX" },
      picked: "INBOX/a.note",
      search: null,
    });
  });

  it("is the search when the note came from its results", () => {
    expect(noteOrigin(none, { selection: { kind: "all" }, picked: null }, "plan")).toEqual({
      selection: { kind: "all" },
      picked: null,
      search: "plan",
    });
  });

  it("stays the place before the first note when a note opens over a note", () => {
    const first = noteOrigin(none, { selection: { kind: "folder", path: "INBOX" }, picked: "INBOX/a.note" }, null);
    const over = { selection: { kind: "note", path: "INBOX/a.note" } as Selection, picked: null };
    // A search hit opened over the review: Back gives the results, then the place.
    expect(noteOrigin(first, over, "plan")).toEqual({ ...first, search: "plan" });
    // A note opened over a note from elsewhere (a notification): no search to give back.
    expect(noteOrigin({ ...first, search: "plan" }, over, null)).toEqual(first);
  });

  it("is where Back goes, after the reveal too", () => {
    const from = noteOrigin(none, { selection: { kind: "folder", path: "INBOX" }, picked: "INBOX/a.note" }, "x");
    expect(afterBack(from)).toEqual(from);
  });
});
