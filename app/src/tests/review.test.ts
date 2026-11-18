import { beforeEach, describe, expect, it, vi } from "vitest";
import { render } from "svelte/server";
import type { NoteView } from "../lib/library-types";

const calls: { command: string; args: Record<string, unknown> | undefined }[] = [];
const pending = new Map<string, { resolve: (value: unknown) => void; reject: (error: unknown) => void }>();
const answers = new Map<string, ((args: Record<string, unknown> | undefined) => Promise<unknown>)[]>();

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn((command: string, args?: Record<string, unknown>) => {
    calls.push({ command, args });
    const next = answers.get(command)?.shift();
    if (next) return next(args);
    if (command === "note_page_image") {
      // Every image waits until the test answers it by its index.
      return new Promise((resolve, reject) => pending.set(String(args?.index), { resolve, reject }));
    }
    return Promise.resolve(null);
  }),
}));

const { reviewPages, pageAt, syncTop, reviewNotice, nearPages, noteTitle, syncGuard, concernsNote, findBlock, keepsHit, PORTRAIT } =
  await import("../library/review");
const { ReviewSession } = await import("../library/session.svelte");
const { openAt } = await import("../library/view");
const ReviewPages = (await import("../library/ReviewPages.svelte")).default;

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

function answerNow(command: string, value: unknown) {
  const list = answers.get(command) ?? [];
  list.push(() => Promise.resolve(value));
  answers.set(command, list);
}

function answerError(command: string, error: unknown) {
  const list = answers.get(command) ?? [];
  list.push(() => Promise.reject(error));
  answers.set(command, list);
}

const PATH = "NOTE/Note/Work/Plan.note";

function view(states: NoteView["pages"][number]["state"][], catchingUp = false): NoteView {
  return {
    key: "k1",
    md5: "m1",
    path: PATH,
    title: "Plan",
    catchingUp,
    pages: states.map((state, index) => {
      const markdown = state === "done" ? `# Page ${index + 1}` : "";
      return {
        index,
        pageId: `P${index}`,
        state,
        markdown,
        blocks: markdown === "" ? [] : [{ id: `b${index}`, text: markdown }],
        landscape: false,
      };
    }),
  };
}

function asked(command: string): number[] {
  return calls.filter((c) => c.command === command).map((c) => c.args?.index as number);
}

beforeEach(() => {
  calls.length = 0;
  pending.clear();
  answers.clear();
});

describe("reviewPages", () => {
  it("gives each page its text, a plain line for its state and a placeholder ratio", () => {
    const rows = reviewPages(view(["done", "pending", "failed", "truncated"]));
    expect(rows.map((r) => r.index)).toEqual([0, 1, 2, 3]);
    expect(rows.map((r) => r.label)).toEqual(["Page 1", "Page 2", "Page 3", "Page 4"]);
    expect(rows[0].markdown).toBe("# Page 1");
    expect(rows.map((r) => r.stateText)).toEqual([
      null,
      "Waiting for the OCR.",
      "Notedown could not read this page.",
      "The text of this page was cut short.",
    ]);
    // Before an image is loaded a page is a Supernote portrait page (1404 x 1872).
    expect(PORTRAIT).toBeCloseTo(1404 / 1872);
    for (const row of rows) expect(row.ratio).toBeCloseTo(1404 / 1872);
  });

  it("keeps the real ratio once an image was loaded, and takes landscape from the view", () => {
    const note = view(["done", "done", "done"]);
    note.pages[2].landscape = true;
    const rows = reviewPages(note, { 1: 1872 / 1404 });
    expect(rows[0].ratio).toBeCloseTo(1404 / 1872);
    expect(rows[1].ratio).toBeCloseTo(1872 / 1404);
    expect(rows[2].ratio).toBeCloseTo(1872 / 1404);
  });

  it("says when a read page has no text", () => {
    const note = view(["done"]);
    note.pages[0].markdown = "  \n";
    expect(reviewPages(note)[0].stateText).toBe("No text on this page.");
  });

  it("says when Notedown still reads the note", () => {
    expect(reviewNotice(view(["done"], true))).toBe("Notedown is still reading this note.");
    expect(reviewNotice(view(["done"]))).toBeNull();
  });
});

describe("the blocks of the review", () => {
  it("reviewPages carries the page id and the blocks", () => {
    const note = view(["done", "pending"]);
    note.pages[0].blocks = [
      { id: "b1", text: "One" },
      { id: "b2", text: "Two" },
    ];
    const rows = reviewPages(note);
    expect(rows.map((r) => r.pageId)).toEqual(["P0", "P1"]);
    expect(rows[0].blocks).toEqual([
      { id: "b1", text: "One" },
      { id: "b2", text: "Two" },
    ]);
    expect(rows[1].blocks).toEqual([]);
  });

  it("findBlock finds the page and the block", () => {
    const note = view(["done", "done", "done"]);
    note.pages[1].pageId = "P1#2";
    note.pages[1].blocks = [
      { id: "b4", text: "x" },
      { id: "b5", text: "y" },
    ];
    const rows = reviewPages(note);
    expect(findBlock(rows, { pageId: "P1#2", blockId: "b5" })).toEqual({ index: 1, blockId: "b5" });
    // A block that is gone gives its page.
    expect(findBlock(rows, { pageId: "P1#2", blockId: "b9" })).toEqual({ index: 1, blockId: null });
    // A page that is gone gives nothing.
    expect(findBlock(rows, { pageId: "P7", blockId: "b5" })).toBeNull();
  });

  it("the mark of a hit stays while the text side is where the window put it", () => {
    expect(keepsHit(300, 300)).toBe(true);
    expect(keepsHit(300, 300.6)).toBe(true);
    // Any other scroll is the user's: by wheel, keys, the scroll bar or the pages side.
    expect(keepsHit(300, 340)).toBe(false);
    expect(keepsHit(300, 0)).toBe(false);
  });

  it("the text side has one block per pre inside its page", () => {
    const note = view(["done", "pending"]);
    note.pages[0].blocks = [
      { id: "b1", text: "One" },
      { id: "b2", text: "Two" },
    ];
    const body = render(ReviewPages, {
      props: { rows: reviewPages(note), images: {}, onSeen: () => {}, onMeasured: () => {} },
    }).body;
    const text = body.slice(body.indexOf('aria-label="Text"'));
    expect(text.match(/data-page="0"/g)).toHaveLength(1);
    expect(text.match(/data-page="1"/g)).toHaveLength(1);
    expect(text).toMatch(/data-page="0"[\s\S]*<pre[^>]*data-block="b1"[^>]*>One<\/pre>[\s\S]*<pre[^>]*data-block="b2"[^>]*>Two<\/pre>[\s\S]*data-page="1"/);
  });
});

describe("pageAt", () => {
  const offsets = [0, 500, 1100, 1600];

  it("gives the page at a scroll position", () => {
    expect(pageAt(0, offsets)).toBe(0);
    expect(pageAt(499, offsets)).toBe(0);
    expect(pageAt(500, offsets)).toBe(1);
    expect(pageAt(1599, offsets)).toBe(2);
    expect(pageAt(9000, offsets)).toBe(3);
  });

  it("gives the first page above the top and for no pages", () => {
    expect(pageAt(-20, offsets)).toBe(0);
    expect(pageAt(100, [])).toBe(0);
  });

  it("scrolls the other side to the same place of the same page", () => {
    const images = { tops: [0, 1000, 2000], end: 3000 };
    const text = { tops: [0, 200, 600], end: 700 };
    expect(syncTop(0, images, text)).toBe(0);
    expect(syncTop(1500, images, text)).toBe(400);
    expect(syncTop(2500, images, text)).toBe(650);
    expect(syncTop(300, text, images)).toBeCloseTo(1250);
    expect(syncTop(5000, images, text)).toBe(700);
  });
});

describe("noteTitle", () => {
  it("names a note by its file name without .note", () => {
    expect(noteTitle("NOTE/Note/Work/Plan.note")).toBe("Plan");
    expect(noteTitle("INBOX/a.pdf")).toBe("a.pdf");
    expect(noteTitle("Plan.note")).toBe("Plan");
  });
});

describe("syncGuard", () => {
  it("does not follow back the scroll the window set itself", () => {
    const guard = syncGuard();
    expect(guard.follows("text", 100)).toBe(true);
    guard.set("pages", 400);
    // The scroll event of the side set: within a pixel of the value, not followed back.
    expect(guard.follows("pages", 400.5)).toBe(false);
    // The user scrolls it then: followed.
    expect(guard.follows("pages", 420)).toBe(true);
    guard.set("text", 50);
    // A user scroll elsewhere than the set value is followed and ends the guard.
    expect(guard.follows("text", 80)).toBe(true);
    expect(guard.follows("text", 50)).toBe(true);
  });
});

describe("concernsNote", () => {
  it("reloads a review only for an event that names its note", () => {
    expect(concernsNote({ paths: [PATH] }, PATH)).toBe(true);
    expect(concernsNote({ paths: ["NOTE/Note/Other.note", PATH] }, PATH)).toBe(true);
    expect(concernsNote({ paths: ["NOTE/Note/Other.note"] }, PATH)).toBe(false);
    // A change of the lists alone (a note marked seen) names no note.
    expect(concernsNote({ paths: [] }, PATH)).toBe(false);
  });

  it("reloads for an event it cannot read", () => {
    for (const odd of [null, undefined, {}, { paths: "x" }, "notes"]) expect(concernsNote(odd, PATH)).toBe(true);
  });
});

describe("nearPages", () => {
  it("is one page above the view and two below, and keeps four on each side", () => {
    expect(nearPages([4, 5], 20)).toEqual({ near: [3, 7], keep: [0, 9] });
    expect(nearPages([0], 3)).toEqual({ near: [0, 2], keep: [0, 2] });
    expect(nearPages([], 3)).toBeNull();
  });
});

describe("openAt", () => {
  it("reads where Rust opens the library, and nothing else", () => {
    expect(openAt({ at: "note", path: PATH })).toEqual({ kind: "note", path: PATH });
    expect(openAt({ at: "new" })).toEqual({ kind: "new" });
    // The setup opens it at the recycle bin after a first sync that deleted files.
    expect(openAt({ at: "bin" })).toEqual({ kind: "bin" });
    // A click on a sync notification opens it at All notes.
    expect(openAt({ at: "all" })).toEqual({ kind: "all" });
    for (const odd of [null, undefined, "new", "bin", { at: "note" }, { at: "note", path: 3 }, { at: "trash" }, "all"]) {
      expect(openAt(odd)).toBeNull();
    }
  });
});

describe("the review session", () => {
  async function opened(states: NoteView["pages"][number]["state"][] = Array.from({ length: 12 }, () => "done" as const)) {
    answerNow("note_open", view(states));
    const session = new ReviewSession(PATH);
    await session.open();
    return session;
  }

  it("opens the note and marks it seen", async () => {
    const session = await opened();
    expect(calls.slice(0, 2)).toEqual([
      { command: "note_open", args: { path: PATH } },
      { command: "note_seen", args: { key: "k1" } },
    ]);
    expect(session.view?.title).toBe("Plan");
    expect(session.error).toBeNull();
  });

  it("opened at a block, it names the block once: a reload does not scroll again", async () => {
    answerNow("note_open", view(["done", "done"]));
    const session = new ReviewSession(PATH, { pageId: "P1", blockId: "b1" });
    await session.open();
    expect(session.target).toEqual({ index: 1, blockId: "b1" });
    session.target = null;
    answerNow("note_open", view(["done", "done"]));
    await session.reload();
    expect(session.target).toBeNull();
  });

  it("opened at a block of a gone page, it names nothing", async () => {
    answerNow("note_open", view(["done"]));
    const session = new ReviewSession(PATH, { pageId: "P5", blockId: "b1" });
    await session.open();
    expect(session.target).toBeNull();
  });

  it("opened at a block after a failed open, the first good read names it", async () => {
    answerError("note_open", "Notedown could not read the note.");
    const session = new ReviewSession(PATH, { pageId: "P0", blockId: "b9" });
    await session.open();
    expect(session.target).toBeNull();
    answerNow("note_open", view(["done"]));
    await session.reload();
    expect(session.target).toEqual({ index: 0, blockId: null });
  });

  it("shows why a note cannot be opened, and marks nothing", async () => {
    answerError("note_open", "This note is not there anymore.");
    const session = new ReviewSession(PATH);
    await session.open();
    expect(session.view).toBeNull();
    expect(session.error).toBe("This note is not there anymore.");
    expect(asked("note_seen")).toEqual([]);
  });

  it("a page image is asked for when it comes near", async () => {
    const session = await opened();
    expect(asked("note_page_image")).toEqual([]);
    session.seen(0, true);
    // The page in view and two pages ahead.
    expect(asked("note_page_image")).toEqual([0, 1, 2]);
    session.seen(1, true);
    expect(asked("note_page_image")).toEqual([0, 1, 2, 3]);
    expect(session.images[0]).toEqual({ state: "loading" });
    pending.get("0")?.resolve("data:image/png;base64,AAAA");
    await flush();
    expect(session.images[0]).toEqual({ state: "shown", url: "data:image/png;base64,AAAA" });
    // A page that is asked already is not asked again.
    session.seen(0, false);
    session.seen(0, true);
    expect(asked("note_page_image")).toEqual([0, 1, 2, 3]);
  });

  it("drops a page image far away: the call is ended, the URL released, the placeholder back", async () => {
    const session = await opened();
    session.seen(0, true);
    pending.get("0")?.resolve("data:image/png;base64,AAAA");
    await flush();
    session.measured(0, 1872, 1404);
    // Scrolled far down: pages 0 to 2 are far away.
    session.seen(0, false);
    session.seen(9, true);
    expect(asked("note_page_image")).toEqual([0, 1, 2, 8, 9, 10, 11]);
    // Page 0 had its image: released, no call. Pages 1 and 2 still waited: dropped in Rust.
    expect(asked("note_page_drop")).toEqual([1, 2]);
    expect(calls.find((c) => c.command === "note_page_drop")?.args).toEqual({ path: PATH, index: 1 });
    expect(session.images[0]).toBeUndefined();
    expect(session.images[1]).toBeUndefined();
    // The ratio of a page stays known for its placeholder.
    expect(reviewPages(session.view!, session.ratios)[0].ratio).toBeCloseTo(1872 / 1404);
    // A late answer of a dropped page is not shown; Rust's answer to the drop is no failure.
    pending.get("1")?.resolve("data:image/png;base64,LATE");
    pending.get("2")?.reject("The page is not shown anymore.");
    await flush();
    expect(session.images[1]).toBeUndefined();
    expect(session.images[2]).toBeUndefined();
    // Near again: asked again.
    session.seen(9, false);
    session.seen(1, true);
    expect(asked("note_page_image").slice(7)).toEqual([0, 1, 2, 3]);
  });

  it("takes the pages that enter and leave the view at once: a jump asks only near it", async () => {
    const session = await opened(Array.from({ length: 300 }, () => "done" as const));
    session.seen(290, true);
    expect(asked("note_page_image")).toEqual([289, 290, 291, 292]);
    // A jump to the top: page 0 enters and page 290 leaves in one observer callback.
    session.seenMany([
      [0, true],
      [290, false],
    ]);
    expect(asked("note_page_image")).toEqual([289, 290, 291, 292, 0, 1, 2]);
    expect(asked("note_page_drop")).toEqual([289, 290, 291, 292]);
  });

  it("asks again for a page that Rust dropped after it was asked again", async () => {
    const session = await opened();
    session.seen(0, true);
    session.seenMany([
      [0, false],
      [9, true],
    ]);
    session.seenMany([
      [9, false],
      [0, true],
    ]);
    expect(asked("note_page_image").filter((i) => i === 0)).toHaveLength(2);
    // The drop of the first ask came after the second joined it in Rust: that one ends too.
    pending.get("0")?.reject("The page is not shown anymore.");
    await flush();
    expect(asked("note_page_image").filter((i) => i === 0)).toHaveLength(3);
    expect(session.images[0]).toEqual({ state: "loading" });
  });

  it("does not ask again for a dropped page that is kept but not near", async () => {
    const session = await opened();
    session.seen(0, true);
    // Page 2 is kept (no drop) but no longer near.
    session.seenMany([
      [0, false],
      [5, true],
    ]);
    pending.get("2")?.reject("The page is not shown anymore.");
    await flush();
    expect(asked("note_page_image").filter((i) => i === 2)).toHaveLength(1);
    expect(session.images[2]).toBeUndefined();
  });

  it("keeps the pages a little out of view (no ask and drop at the edge)", async () => {
    const session = await opened();
    session.seen(4, true);
    expect(asked("note_page_image")).toEqual([3, 4, 5, 6]);
    session.seen(4, false);
    session.seen(6, true);
    expect(asked("note_page_drop")).toEqual([]);
    expect(asked("note_page_image")).toEqual([3, 4, 5, 6, 7, 8]);
  });

  it("says why a page has no image, and asks again after the window was hidden", async () => {
    const session = await opened();
    session.seen(0, true);
    pending.get("0")?.reject("The page could not be drawn.");
    // Hiding the window closed the review in Rust: no failure, the placeholder stays.
    pending.get("1")?.reject("The review was closed.");
    await flush();
    expect(session.images[0]).toEqual({ state: "failed", text: "The page could not be drawn." });
    expect(session.images[1]).toBeUndefined();
    session.refresh();
    expect(asked("note_page_image")).toEqual([0, 1, 2, 1]);
  });

  it("closes the review in Rust and shows no answer after it", async () => {
    const session = await opened();
    session.seen(0, true);
    session.close();
    expect(calls.at(-1)).toEqual({ command: "note_close", args: { path: PATH } });
    pending.get("0")?.resolve("data:image/png;base64,AAAA");
    await flush();
    expect(session.images[0]).toBeUndefined();
    session.seen(5, true);
    expect(asked("note_page_image")).toEqual([0, 1, 2]);
  });

  /** Answers every page image asked so far with a URL that names the page. */
  async function showAll() {
    for (const [index, call] of pending) call.resolve(`data:image/png;base64,P${index}`);
    pending.clear();
    await flush();
  }

  it("reload keeps the images of the pages that stay", async () => {
    const session = await opened(["done", "pending", "done", "done"]);
    session.seen(0, true);
    session.measured(1, 1872, 1404);
    await showAll();
    expect(asked("note_page_image")).toEqual([0, 1, 2]);
    // The OCR read page 2 and a page was inserted before page 3.
    const next = view(["done", "done", "done", "done", "done"]);
    next.pages[2].pageId = "PX";
    answerNow("note_open", next);
    await session.reload();
    expect(session.view).toEqual(next);
    expect(session.images[0]).toEqual({ state: "shown", url: "data:image/png;base64,P0" });
    expect(session.images[1]).toEqual({ state: "shown", url: "data:image/png;base64,P1" });
    // Another page at index 2: its old image goes and it is asked again.
    expect(session.images[2]).toEqual({ state: "loading" });
    expect(asked("note_page_image")).toEqual([0, 1, 2, 2]);
    // The ratio of a page that stayed is kept.
    expect(reviewPages(session.view!, session.ratios)[1].ratio).toBeCloseTo(1872 / 1404);
  });

  it("reload drops every image when the file is another version", async () => {
    const session = await opened(["done", "done", "done"]);
    session.seen(0, true);
    session.measured(0, 1872, 1404);
    await showAll();
    // The same pages, but more strokes on them: the images are of the old file.
    answerNow("note_open", { ...view(["done", "done", "done"]), md5: "m2" });
    await session.reload();
    expect(session.images).toEqual({ 0: { state: "loading" }, 1: { state: "loading" }, 2: { state: "loading" } });
    expect(asked("note_page_image")).toEqual([0, 1, 2, 0, 1, 2]);
    // The ratio of a page that stayed is kept.
    expect(session.ratios[0]).toBeCloseTo(1872 / 1404);
  });

  it("reload drops images past a shorter note", async () => {
    const session = await opened(["done", "done", "done"]);
    session.seen(0, true);
    await showAll();
    answerNow("note_open", view(["done", "done"]));
    await session.reload();
    expect(session.view?.pages.length).toBe(2);
    expect(Object.keys(session.images)).toEqual(["0", "1"]);
    expect(asked("note_page_drop")).toEqual([]);
  });

  it("reload ends the call of a page that went while it loads", async () => {
    const session = await opened(["done", "done", "done"]);
    session.seen(0, true);
    answerNow("note_open", view(["done"]));
    await session.reload();
    expect(asked("note_page_drop")).toEqual([1, 2]);
    expect(Object.keys(session.images)).toEqual(["0"]);
  });

  it("a failed reload keeps the view and says why", async () => {
    const session = await opened(["done", "pending"]);
    const before = session.view;
    expect(session.stale).toBeNull();
    answerError("note_open", "This note is not there anymore.");
    await session.reload();
    expect(session.view).toBe(before);
    expect(session.error).toBeNull();
    expect(session.stale).toBe("This view is not up to date. This note is not there anymore.");
    // A reload that works again ends the line.
    answerNow("note_open", view(["done", "pending"]));
    await session.reload();
    expect(session.stale).toBeNull();
  });

  it("asks for no new page image while the view is not up to date", async () => {
    const session = await opened();
    session.seen(0, true);
    expect(asked("note_page_image")).toEqual([0, 1, 2]);
    answerError("note_open", "The connection to the server was cut.");
    await session.reload();
    // Far down: the pages there would come from a file the text may not match.
    session.seen(0, false);
    session.seen(9, true);
    expect(asked("note_page_image")).toEqual([0, 1, 2]);
    // Far pages still go.
    expect(asked("note_page_drop")).toEqual([0, 1, 2]);
    answerNow("note_open", view(Array.from({ length: 12 }, () => "done" as const)));
    await session.reload();
    expect(asked("note_page_image")).toEqual([0, 1, 2, 8, 9, 10, 11]);
  });

  it("tries a failed open again when the window gets the focus", async () => {
    answerError("note_open", "The file did not arrive whole. Try again.");
    const session = new ReviewSession(PATH);
    await session.open();
    expect(session.error).toBe("The file did not arrive whole. Try again.");
    answerNow("note_open", view(["done"]));
    session.refresh();
    await flush();
    expect(session.error).toBeNull();
    expect(session.view?.pages).toHaveLength(1);
    expect(calls.filter((c) => c.command === "note_open")).toHaveLength(2);
    // With a view that is up to date, the focus reads nothing again.
    session.refresh();
    await flush();
    expect(calls.filter((c) => c.command === "note_open")).toHaveLength(2);
  });

  it("a failed open has a Try Again button that opens the note again", async () => {
    const source = (await import("../library/Review.svelte?raw")).default as string;
    expect(source).toContain('onclick={() => void session.reload()}>Try Again</button>');
    answerError("note_open", "The file did not arrive whole. Try again.");
    const session = new ReviewSession(PATH);
    await session.open();
    answerNow("note_open", view(["done"]));
    await session.reload();
    expect(session.error).toBeNull();
    expect(asked("note_seen")).toHaveLength(1);
  });

  it("tries a failed reload again when the window gets the focus", async () => {
    const session = await opened(["done"]);
    answerError("note_open", "The connection to the server was cut.");
    await session.reload();
    expect(session.stale).not.toBeNull();
    answerNow("note_open", view(["done"]));
    session.refresh();
    await flush();
    expect(session.stale).toBeNull();
    expect(calls.filter((c) => c.command === "note_open")).toHaveLength(3);
  });

  it("a reload the window ended itself is no reason to show", async () => {
    const session = await opened(["done"]);
    answerError("note_open", "The review was closed.");
    await session.reload();
    expect(session.stale).toBeNull();
  });

  it("marks new text seen after a reload, and nothing without new text", async () => {
    const session = await opened(["done", "pending"]);
    expect(asked("note_seen")).toHaveLength(1);
    answerNow("note_open", view(["done", "pending"]));
    await session.reload();
    expect(asked("note_seen")).toHaveLength(1);
    answerNow("note_open", view(["done", "done"]));
    await session.reload();
    expect(asked("note_seen")).toHaveLength(2);
    expect(calls.at(-1)).toEqual({ command: "note_seen", args: { key: "k1" } });
  });

  it("no note_seen for a note without a key", async () => {
    const unread = { ...view(["pending", "pending"]), key: null };
    answerNow("note_open", unread);
    const session = new ReviewSession(PATH);
    await session.open();
    expect(session.view).toEqual(unread);
    answerNow("note_open", { ...view(["done", "pending"]), key: null });
    await session.reload();
    expect(session.view?.pages[0].markdown).toBe("# Page 1");
    expect(asked("note_seen")).toEqual([]);
    // The store came to know the note: its text is seen.
    answerNow("note_open", view(["done", "done"]));
    await session.reload();
    expect(calls.filter((c) => c.command === "note_seen").map((c) => c.args)).toEqual([{ key: "k1" }]);
  });

  it("reloads asked for while one runs give one more, and an older answer never wins", async () => {
    const session = await opened(["pending"]);
    const releases: ((value: unknown) => void)[] = [];
    const held = () => new Promise((resolve) => releases.push(resolve));
    answers.set("note_open", [held, held, held]);
    const first = session.reload();
    const second = session.reload();
    const third = session.reload();
    expect(calls.filter((c) => c.command === "note_open")).toHaveLength(2);
    releases[0](view(["done"]));
    await flush();
    expect(calls.filter((c) => c.command === "note_open")).toHaveLength(3);
    releases[1](view(["done", "done"]));
    await Promise.all([first, second, third]);
    expect(calls.filter((c) => c.command === "note_open")).toHaveLength(3);
    expect(session.view?.pages.length).toBe(2);
  });

  it("a reload after the close does nothing", async () => {
    const session = await opened(["pending"]);
    session.close();
    await session.reload();
    expect(calls.filter((c) => c.command === "note_open")).toHaveLength(1);
  });

  it("a note opened while the session closed is not marked seen", async () => {
    let release: (value: unknown) => void = () => {};
    answers.set("note_open", [() => new Promise((resolve) => (release = resolve))]);
    const session = new ReviewSession(PATH);
    const opening = session.open();
    session.close();
    release(view(["done"]));
    await opening;
    expect(asked("note_seen")).toEqual([]);
    expect(session.view).toBeNull();
  });
});

describe("note text", () => {
  it("stays text: no HTML from a note is inserted", () => {
    const note = view(["done", "done"]);
    note.pages[0].markdown = '<img src=x onerror="alert(1)"> and <script>alert(2)</script>';
    note.pages[0].blocks = [{ id: "b0", text: note.pages[0].markdown }];
    note.pages[1].markdown = "**bold** [link](javascript:alert(3))";
    note.pages[1].blocks = [{ id: "b1", text: note.pages[1].markdown }];
    const body = render(ReviewPages, {
      props: {
        rows: reviewPages(note),
        images: {},
        onSeen: () => {},
        onMeasured: () => {},
      },
    }).body;
    expect(body).toContain("&lt;img src=x onerror=");
    expect(body).toContain("&lt;script>alert(2)&lt;/script>");
    expect(body).not.toContain("<img src=x");
    expect(body).not.toContain("<script>");
    expect(body).not.toContain('href="javascript');
    expect(body).toContain("**bold** [link](javascript:alert(3))");
  });

  it("is never inserted as HTML by a component", () => {
    const sources = import.meta.glob(["../library/*.svelte", "../panel/*.svelte"], {
      query: "?raw",
      import: "default",
      eager: true,
    });
    expect(Object.keys(sources)).toContain("../library/ReviewPages.svelte");
    for (const [file, source] of Object.entries(sources)) expect(source, file).not.toContain("{@html");
  });
});
