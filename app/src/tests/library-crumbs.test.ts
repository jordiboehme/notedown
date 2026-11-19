import { describe, expect, it, vi } from "vitest";
import { render } from "svelte/server";
import Toolbar from "../library/Toolbar.svelte";
import FolderList from "../library/FolderList.svelte";
import { LibrarySearch } from "../library/search.svelte";
import { toolbarPath, type Selection } from "../library/view";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(() => Promise.resolve(null)) }));

describe("the path in the toolbar", () => {
  it("names each folder of a shown folder, every one a button", () => {
    expect(toolbarPath({ kind: "folder", path: "NOTE/Note/Work/2026" })).toEqual({
      crumbs: [
        { label: "Note", path: "NOTE/Note" },
        { label: "Work", path: "NOTE/Note/Work" },
        { label: "2026", path: "NOTE/Note/Work/2026" },
      ],
      full: "Note / Work / 2026",
    });
  });

  it("ends with the open note's name, which is no button", () => {
    expect(toolbarPath({ kind: "note", path: "DOCUMENT/Document/a/Plan.note" })).toEqual({
      crumbs: [
        { label: "Document", path: "DOCUMENT/Document" },
        { label: "a", path: "DOCUMENT/Document/a" },
        { label: "Plan", path: null },
      ],
      full: "Document / a / Plan",
    });
  });

  it("is the own name of New, All notes and the bin", () => {
    const named = (kind: "new" | "all" | "bin") => toolbarPath({ kind } as Selection);
    expect(named("new")).toEqual({ crumbs: [{ label: "New", path: null }], full: "New" });
    expect(named("all")).toEqual({ crumbs: [{ label: "All notes", path: null }], full: "All notes" });
    expect(named("bin")).toEqual({ crumbs: [{ label: "Recycle bin", path: null }], full: "Recycle bin" });
  });
});

function toolbar(selection: Selection) {
  return render(Toolbar, {
    props: {
      view: "tree",
      onView: () => {},
      search: new LibrarySearch(() => ({ kind: "other" })),
      path: toolbarPath(selection),
      onOpen: () => {},
    },
  }).body;
}

describe("the toolbar", () => {
  it("shows the path with the folders as buttons and the full path as its tooltip", () => {
    const body = toolbar({ kind: "note", path: "NOTE/Note/Work/Plan.note" });
    const path = body.match(/<nav[^>]*class="path[^"]*"[^>]*>/)?.[0] ?? "";
    expect(path).toContain('title="Note / Work / Plan"');
    const buttons = [...body.matchAll(/<button[^>]*class="crumb[^"]*"[^>]*>([^<]*)<\/button>/g)].map((m) => m[1]);
    expect(buttons).toEqual(["Note", "Work"]);
    expect(body).toMatch(/<span[^>]*class="crumb[^"]*"[^>]*>Plan<\/span>/);
  });

  it("shows the shown folder as a button too", () => {
    const body = toolbar({ kind: "folder", path: "NOTE/Note/Work" });
    const buttons = [...body.matchAll(/<button[^>]*class="crumb[^"]*"[^>]*>([^<]*)<\/button>/g)].map((m) => m[1]);
    expect(buttons).toEqual(["Note", "Work"]);
  });

  it("is the only path: the folder list has none of its own", () => {
    const body = render(FolderList, {
      props: {
        path: "NOTE/Note/Work",
        folder: { path: "NOTE/Note/Work", entries: [], complete: true, loading: false, error: null },
        next: null,
        busy: false,
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
    expect(body).not.toContain('aria-label="Path"');
  });
});
