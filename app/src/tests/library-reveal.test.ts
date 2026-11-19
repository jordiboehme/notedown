import { describe, expect, it } from "vitest";
import { afterBack, revealOf, rootRows, treeSelected, treeView, type Reveal, type Selection } from "../library/view";

describe("the reveal of an open note", () => {
  it("is the note's folder and the folders above it", () => {
    expect(revealOf("NOTE/Note/Work/2026/Plan.note")).toEqual({
      note: "NOTE/Note/Work/2026/Plan.note",
      folder: "NOTE/Note/Work/2026",
      opens: ["NOTE/Note", "NOTE/Note/Work"],
    });
  });

  it("is the device tree itself for a note right in it", () => {
    expect(revealOf("NOTE/Note/Plan.note")).toEqual({ note: "NOTE/Note/Plan.note", folder: "NOTE/Note", opens: [] });
    expect(revealOf("DOCUMENT/Document/a/b.note")).toEqual({
      note: "DOCUMENT/Document/a/b.note",
      folder: "DOCUMENT/Document/a",
      opens: ["DOCUMENT/Document"],
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
  const reveal: Reveal = { note: "NOTE/Note/Work/Plan.note", folder: "NOTE/Note/Work", opens: ["NOTE/Note"] };

  it("keeps the note's folder selected, with the note picked", () => {
    expect(afterBack({ kind: "new" }, "NOTE/Note/Work/Plan.note", reveal)).toEqual({
      selection: { kind: "folder", path: "NOTE/Note/Work" },
      picked: "NOTE/Note/Work/Plan.note",
    });
  });

  it("goes where it came from when the note was not revealed", () => {
    expect(afterBack({ kind: "new" }, "NOTE/Note/Work/Plan.note", null)).toEqual({ selection: { kind: "new" }, picked: null });
    // A reveal of another note does not count.
    expect(afterBack({ kind: "bin" }, "NOTE/Note/Other.note", reveal)).toEqual({ selection: { kind: "bin" }, picked: null });
  });
});
