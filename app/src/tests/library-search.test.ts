import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ContentHit, Entry, Found, NoteSummary } from "../lib/library-types";

const calls: { command: string; args: Record<string, unknown> | undefined }[] = [];
const answers = new Map<string, ((args: Record<string, unknown> | undefined) => Promise<unknown>)[]>();

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn((command: string, args?: Record<string, unknown>) => {
    calls.push({ command, args });
    const next = answers.get(command)?.shift();
    if (next) return next(args);
    if (command === "library_find") return Promise.resolve({ entries: [], more: false });
    return Promise.resolve([]);
  }),
}));

const { DEBOUNCE_MS, SCOPE_LABELS, effectiveScope, entryRow, fieldTakesEscape, filterNotes, isFindKey, resultRows, searchPlace, wantsContent } =
  await import("../library/search");
const { withoutAt } = await import("../library/view");
const SearchResults = (await import("../library/SearchResults.svelte")).default;
const SearchLayer = (await import("../library/SearchLayer.svelte")).default;
const { render } = await import("svelte/server");
const { createRawSnippet } = await import("svelte");
const { LibrarySearch } = await import("../library/search.svelte");
type SearchPlace = import("../library/search.svelte").SearchPlace;

function note(key: string, path: string): NoteSummary {
  const title = path.slice(path.lastIndexOf("/") + 1).replace(/\.note$/, "");
  return { key, path, title, pages: 1, pagesWithText: 1, changedMs: 0 };
}

function entry(path: string): Entry {
  return {
    id: 1,
    path,
    name: path.slice(path.lastIndexOf("/") + 1),
    kind: "file",
    size: 1,
    md5: null,
    mtimeMs: 0,
    writable: true,
  };
}

function hit(key: string, path: string, context = "a line"): ContentHit {
  return { key, path, title: key, page: 1, pageId: "P1", blockId: "b1", context };
}

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

const commands = () => calls.map((c) => c.command);
/** Lets the answers that are already there reach the search. */
const settle = () => vi.advanceTimersByTimeAsync(0);

let place: SearchPlace = { kind: "folder", path: "NOTE/Note" };
const search = () => new LibrarySearch(() => place);

beforeEach(() => {
  vi.useFakeTimers();
  calls.length = 0;
  answers.clear();
  place = { kind: "folder", path: "NOTE/Note" };
});

afterEach(() => {
  vi.useRealTimers();
});

describe("the pure parts", () => {
  it("filterNotes is case-insensitive", () => {
    const notes = [note("A", "NOTE/Note/Plan.note"), note("B", "NOTE/Note/shop.note")];
    expect(filterNotes(notes, "PLAN").map((n) => n.key)).toEqual(["A"]);
    expect(filterNotes(notes, "plan").map((n) => n.key)).toEqual(["A"]);
    expect(filterNotes(notes, "")).toEqual([]);
  });

  it("wantsContent needs two characters", () => {
    expect(wantsContent("a")).toBe(false);
    expect(wantsContent(" a ")).toBe(false);
    expect(wantsContent("ab")).toBe(true);
  });

  it("resultRows puts names first and keeps a note in both lists", () => {
    const rows = resultRows(
      [{ kind: "note", path: "NOTE/Note/a.note", name: "a.note" }],
      [hit("A", "NOTE/Note/a.note", "plan here")],
    );
    expect(rows.map((r) => [r.section, r.path])).toEqual([
      ["names", "NOTE/Note/a.note"],
      ["text", "NOTE/Note/a.note"],
    ]);
    expect(rows[1]).toMatchObject({ context: "plan here", page: 1, pageId: "P1", blockId: "b1" });
  });

  it("Cmd+F is the find key", () => {
    expect(isFindKey({ key: "f", metaKey: true })).toBe(true);
    expect(isFindKey({ key: "F", metaKey: true })).toBe(true);
    expect(isFindKey({ key: "f", metaKey: false })).toBe(false);
    expect(isFindKey({ key: "g", metaKey: true })).toBe(false);
  });

  it("the place of the search follows the selection", () => {
    const notes = [note("A", "NOTE/Note/a.note")];
    expect(searchPlace({ kind: "new" }, notes, null)).toEqual({ kind: "new", notes });
    expect(searchPlace({ kind: "all" }, null, notes)).toEqual({ kind: "all", notes });
    expect(searchPlace({ kind: "folder", path: "NOTE" }, null, null)).toEqual({ kind: "folder", path: "NOTE" });
    expect(searchPlace({ kind: "bin" }, null, null)).toEqual({ kind: "other" });
    expect(searchPlace({ kind: "note", path: "NOTE/a.note" }, null, null)).toEqual({ kind: "other" });
  });

  it("This folder is only there where a folder or a list of notes is shown", () => {
    const folder: SearchPlace = { kind: "folder", path: "NOTE" };
    const other: SearchPlace = { kind: "other" };
    expect(effectiveScope("here", folder)).toBe("here");
    expect(effectiveScope("here", { kind: "new", notes: [] })).toBe("here");
    expect(effectiveScope("here", other)).toBe("everywhere");
    expect(effectiveScope("everywhere", folder)).toBe("everywhere");
  });

  it("a name row is a note or a file", () => {
    expect(entryRow(entry("NOTE/a.note")).kind).toBe("note");
    expect(entryRow(entry("Document/a.pdf")).kind).toBe("file");
  });

  it("a note opened at a block loses the block once it was used", () => {
    const at = { pageId: "P1", blockId: "b1" };
    expect(withoutAt({ kind: "note", path: "NOTE/a.note", at })).toEqual({ kind: "note", path: "NOTE/a.note" });
    const plain = { kind: "note" as const, path: "NOTE/a.note" };
    expect(withoutAt(plain)).toBe(plain);
    const folder = { kind: "folder" as const, path: "NOTE" };
    expect(withoutAt(folder)).toBe(folder);
  });

  it("the scope labels and the wait are the plan's", () => {
    expect(SCOPE_LABELS).toEqual({ here: "This folder", everywhere: "Everywhere" });
    expect(DEBOUNCE_MS).toBe(200);
  });
});

describe("LibrarySearch", () => {
  it("starts with scope Everywhere and no rows", () => {
    const s = search();
    expect(s.scope).toBe("everywhere");
    expect(s.rows).toEqual([]);
  });

  it("typing fast makes one call after 200 ms", async () => {
    const s = search();
    s.type("p");
    s.type("pl");
    s.type("pla");
    s.type("plan");
    await vi.advanceTimersByTimeAsync(199);
    expect(calls).toEqual([]);
    await vi.advanceTimersByTimeAsync(1);
    expect(commands().sort()).toEqual(["library_find", "notes_search"]);
    expect(calls.find((c) => c.command === "library_find")?.args).toEqual({ text: "plan", under: "" });
    expect(calls.find((c) => c.command === "notes_search")?.args).toEqual({ query: "plan", under: null });
  });

  it("shows the answers as rows, names first", async () => {
    answerNow("library_find", { entries: [entry("NOTE/Note/plan.note"), entry("Document/plan.pdf")], more: false });
    answerNow("notes_search", [hit("A", "NOTE/Note/x.note")]);
    const s = search();
    s.type("plan");
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    expect(s.rows.map((r) => [r.section, r.kind, r.path])).toEqual([
      ["names", "note", "NOTE/Note/plan.note"],
      ["names", "file", "Document/plan.pdf"],
      ["text", "note", "NOTE/Note/x.note"],
    ]);
    expect(s.busy).toBe(false);
  });

  it("an older answer does not replace a newer one", async () => {
    const firstNames = answerLater("library_find");
    const firstText = answerLater("notes_search");
    const secondNames = answerLater("library_find");
    const secondText = answerLater("notes_search");
    const s = search();
    s.type("plan");
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    s.type("plant");
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    expect(s.busy).toBe(true);
    secondNames.release({ entries: [entry("NOTE/plant.note")], more: false } satisfies Found);
    secondText.release([hit("B", "NOTE/b.note")]);
    await settle();
    firstNames.release({ entries: [entry("NOTE/plan.note")], more: true } satisfies Found);
    firstText.release([hit("A", "NOTE/a.note")]);
    await settle();
    expect(s.rows.map((r) => r.path)).toEqual(["NOTE/plant.note", "NOTE/b.note"]);
    expect(s.more).toBe(false);
    expect(s.busy).toBe(false);
  });

  it("one character searches names only", async () => {
    const s = search();
    s.type("p");
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    expect(commands()).toEqual(["library_find"]);
  });

  it("a query of only punctuation gives no text rows and no error line", async () => {
    const s = search();
    s.type("--");
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    expect(commands()).toEqual(["library_find"]);
    expect(s.rows.filter((r) => r.section === "text")).toEqual([]);
    expect(s.error).toBeNull();
  });

  it("clear drops the rows and ends pending calls", async () => {
    answerNow("library_find", { entries: [entry("NOTE/plan.note")], more: false });
    const s = search();
    s.type("plan");
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    expect(s.rows).toHaveLength(1);
    const late = answerLater("library_find");
    s.type("plans");
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    s.type("plans!");
    s.clear();
    // The waiting keystroke never runs, and the answer of the run before it is dropped.
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS * 2);
    late.release({ entries: [entry("NOTE/plans.note")], more: false });
    await settle();
    expect(s.text).toBe("");
    expect(s.rows).toEqual([]);
    expect(s.busy).toBe(false);
    expect(commands().filter((c) => c === "library_find")).toHaveLength(2);
  });

  it("scope here passes the shown folder as under", async () => {
    const s = search();
    s.setScope("here");
    s.type("plan");
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    expect(calls.find((c) => c.command === "library_find")?.args).toEqual({ text: "plan", under: "NOTE/Note" });
    expect(calls.find((c) => c.command === "notes_search")?.args).toEqual({ query: "plan", under: "NOTE/Note" });
  });

  it("scope here in New names the new notes at once and keeps only content hits of new notes", async () => {
    place = { kind: "new", notes: [note("A", "NOTE/Note/Plan.note"), note("B", "NOTE/Note/b.note")] };
    answerNow("notes_search", [hit("C", "NOTE/Note/c.note"), hit("B", "NOTE/Note/b.note")]);
    const s = search();
    s.setScope("here");
    s.type("plan");
    // The names come from the list at once.
    expect(s.rows.map((r) => [r.section, r.path])).toEqual([["names", "NOTE/Note/Plan.note"]]);
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    expect(commands()).toEqual(["notes_search"]);
    expect(calls[0].args).toEqual({ query: "plan", under: null });
    expect(s.rows.map((r) => [r.section, r.path])).toEqual([
      ["names", "NOTE/Note/Plan.note"],
      ["text", "NOTE/Note/b.note"],
    ]);
  });

  it("Everywhere asks library_find even in New", async () => {
    place = { kind: "new", notes: [note("A", "NOTE/Note/Plan.note")] };
    const s = search();
    s.type("plan");
    expect(s.rows).toEqual([]);
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    expect(calls.find((c) => c.command === "library_find")?.args).toEqual({ text: "plan", under: "" });
  });

  it("a failed call shows its text and the other list stays", async () => {
    answerNow("library_find", { entries: [entry("NOTE/plan.note")], more: true });
    const failing = answerLater("notes_search");
    const s = search();
    s.type("plan");
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    failing.fail("Notedown could not search the text of the notes.");
    await settle();
    expect(s.error).toBe("Notedown could not search the text of the notes.");
    expect(s.rows.map((r) => r.path)).toEqual(["NOTE/plan.note"]);
    expect(s.more).toBe(true);
    // The next run clears the line.
    s.type("plans");
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    expect(s.error).toBeNull();
  });

  it("This folder in the review searches everywhere and is not the scope it shows", async () => {
    place = { kind: "other" };
    const s = search();
    s.setScope("here");
    expect(s.shownScope).toBe("everywhere");
    expect(s.hereAvailable).toBe(false);
    s.type("plan");
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    expect(calls.find((c) => c.command === "library_find")?.args).toEqual({ text: "plan", under: "" });
  });

  it("a list that is still loading shows no false empty text and waits for the list", async () => {
    place = { kind: "new", notes: null };
    const s = search();
    s.setScope("here");
    s.type("plan");
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS * 2);
    expect(calls).toEqual([]);
    expect(s.busy).toBe(true);
    expect(s.rows).toEqual([]);
    // The list comes: the search runs for it.
    place = { kind: "new", notes: [note("A", "NOTE/Note/Plan.note")] };
    s.rerun();
    expect(s.rows.map((r) => r.path)).toEqual(["NOTE/Note/Plan.note"]);
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    expect(commands()).toEqual(["notes_search"]);
    expect(s.busy).toBe(false);
  });

  it("a list that fails to load ends the wait and shows the error", async () => {
    place = { kind: "new", notes: null };
    const s = search();
    s.setScope("here");
    s.type("plan");
    expect(s.busy).toBe(true);
    // The list fails: the search stops waiting and says why.
    place = { kind: "new", notes: null, error: "store gone" };
    s.rerun();
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS * 2);
    expect(calls).toEqual([]);
    expect(s.busy).toBe(false);
    expect(s.error).toBe("store gone");
    expect(s.rows).toEqual([]);
    // The list comes later: the search runs for it.
    place = { kind: "new", notes: [note("A", "NOTE/Note/Plan.note")] };
    s.rerun();
    expect(s.error).toBeNull();
    expect(s.rows.map((r) => r.path)).toEqual(["NOTE/Note/Plan.note"]);
  });

  it("the place of New or All notes carries the error of a list that failed", () => {
    expect(searchPlace({ kind: "new" }, null, null, { newNotes: "gone" })).toEqual({ kind: "new", notes: null, error: "gone" });
    expect(searchPlace({ kind: "all" }, null, null, { allNotes: "gone" })).toEqual({ kind: "all", notes: null, error: "gone" });
    expect(searchPlace({ kind: "all" }, null, null, { newNotes: "gone" })).toEqual({ kind: "all", notes: null });
  });

  it("the field takes Escape only while it holds text", () => {
    expect(fieldTakesEscape("plan")).toBe(true);
    expect(fieldTakesEscape(" ")).toBe(true);
    expect(fieldTakesEscape("")).toBe(false);
  });

  it("a list that changes while This folder searches it is filtered again; the same place runs nothing", async () => {
    place = { kind: "all", notes: [note("A", "NOTE/Note/Plan.note")] };
    const s = search();
    s.setScope("here");
    s.type("plan");
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    calls.length = 0;
    s.rerun();
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    expect(calls).toEqual([]);
    place = { kind: "all", notes: [note("A", "NOTE/Note/Plan.note"), note("B", "NOTE/Note/plans.note")] };
    s.rerun();
    expect(s.rows.map((r) => r.path)).toEqual(["NOTE/Note/Plan.note", "NOTE/Note/plans.note"]);
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    expect(commands()).toEqual(["notes_search"]);
  });

  it("a list that changes while Everywhere searches runs nothing", async () => {
    place = { kind: "all", notes: [note("A", "NOTE/Note/Plan.note")] };
    const s = search();
    s.type("plan");
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    calls.length = 0;
    place = { kind: "all", notes: [note("B", "NOTE/Note/b.note")] };
    s.rerun();
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    expect(calls).toEqual([]);
  });

  it("a new scope runs again", async () => {
    const s = search();
    s.type("plan");
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    calls.length = 0;
    s.setScope("here");
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    expect(calls.find((c) => c.command === "library_find")?.args).toEqual({ text: "plan", under: "NOTE/Note" });
  });
});

describe("the search results", () => {
  function body(s: InstanceType<typeof LibrarySearch>) {
    return render(SearchResults, { props: { search: s, onRow: () => {} } }).body;
  }

  it("show the headings, the empty texts and the more line", () => {
    const s = search();
    s.text = "plan";
    let html = body(s);
    expect(html).toContain("Names");
    expect(html).toContain("In the Text");
    expect(html).toContain("No names match.");
    expect(html).toContain("No text matches.");
    expect(html).not.toContain("More than 200 names.");
    s.names = [{ kind: "file", path: "Document/plan.pdf", name: "plan.pdf" }];
    s.more = true;
    html = body(s);
    expect(html).not.toContain("No names match.");
    expect(html).toContain("More than 200 names. Type more to narrow the list.");
    s.text = "p";
    expect(body(s)).toContain("Type 2 characters or more to search the text.");
  });

  it("show This folder as not available in the review and the bin", () => {
    place = { kind: "other" };
    const s = search();
    s.text = "plan";
    s.setScope("here");
    const here = body(s).match(/<button[^>]*>\s*This folder/)?.[0] ?? "";
    expect(here).toContain("disabled");
    expect(here).toContain('aria-pressed="false"');
    place = { kind: "folder", path: "NOTE" };
    const shown = body(s).match(/<button[^>]*>\s*This folder/)?.[0] ?? "";
    expect(shown).not.toContain("disabled");
    expect(shown).toContain('aria-pressed="true"');
  });
});

describe("the search layer", () => {
  const view = createRawSnippet(() => ({ render: () => '<p class="the-view">The review</p>' }));
  const results = createRawSnippet(() => ({ render: () => '<p class="the-results">Results</p>' }));

  it("keeps the view mounted under the results and makes it inert", () => {
    const searching = render(SearchLayer, { props: { searching: true, view, results } }).body;
    expect(searching).toContain("The review");
    expect(searching).toContain("Results");
    expect(searching).toMatch(/<div[^>]*class="view[^"]*"[^>]*inert/);
    const idle = render(SearchLayer, { props: { searching: false, view, results } }).body;
    expect(idle).toContain("The review");
    expect(idle).not.toContain("Results");
    expect(idle).not.toMatch(/inert/);
  });
});
