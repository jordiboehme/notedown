import { beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { pageMenu } from "../library/review";
import { WebMenuGuard, keepsWebMenu, selectionCopy, type MenuSelection, type MenuTarget } from "../library/webmenu";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(() => Promise.resolve(null)) }));

describe("the page menu", () => {
  beforeEach(() => vi.mocked(invoke).mockClear());

  it("is Rust's, opened on the page at the pointer, instead of WebKit's", () => {
    const preventDefault = vi.fn();
    pageMenu({ preventDefault, clientX: 120, clientY: 48.5 }, "NOTE/Note/Plan.note", 2);
    expect(preventDefault).toHaveBeenCalled();
    expect(invoke).toHaveBeenCalledWith("library_menu", {
      target: { kind: "page", path: "NOTE/Note/Plan.note", index: 2, x: 120, y: 48.5 },
    });
  });
});

/** An element of the window: `field` is what `closest` finds for a text field. */
function element(field: { type?: string; editable?: boolean } | null = null): MenuTarget {
  return {
    closest: (selector: string) => {
      if (!field) return null;
      expect(selector).toContain("input");
      return { type: field.type ?? "", isContentEditable: field.editable ?? false } as never;
    },
  } as MenuTarget;
}

function selection(text: string, holds: boolean): MenuSelection {
  return { text, holds: () => holds };
}

describe("WebKit's own menu", () => {
  it("stays in text fields, for cut, copy and paste", () => {
    expect(keepsWebMenu(element({ type: "text" }), null)).toBe(true);
    expect(keepsWebMenu(element({ type: "search" }), null)).toBe(true);
    expect(keepsWebMenu(element({ type: "" }), null)).toBe(true);
    expect(keepsWebMenu(element({ editable: true }), null)).toBe(true);
  });

  it("stays on selected text, for copy", () => {
    expect(keepsWebMenu(element(), selection("Hello", true))).toBe(true);
  });

  it("goes everywhere else", () => {
    expect(keepsWebMenu(element(), null)).toBe(false);
    expect(keepsWebMenu(element(), selection("", false))).toBe(false);
    // Text is selected, but somewhere else.
    expect(keepsWebMenu(element(), selection("Hello", false))).toBe(false);
    // Only blanks are no text to copy.
    expect(keepsWebMenu(element(), selection("  \n", true))).toBe(false);
    // A button or a checkbox is no text field.
    expect(keepsWebMenu(element({ type: "checkbox" }), null)).toBe(false);
    expect(keepsWebMenu(element({ type: "button" }), null)).toBe(false);
    expect(keepsWebMenu(null, null)).toBe(false);
  });
});

/** A range of a fake selection: it holds the nodes in `nodes` and the points in `points`. */
function range(nodes: unknown[], points: [unknown, number][] = []) {
  const made = {
    cloned: 0,
    cloneRange() {
      made.cloned += 1;
      return made;
    },
    intersectsNode: (node: unknown) => nodes.includes(node),
    isPointInRange: (node: unknown, offset: number) => points.some(([n, o]) => n === node && o === offset),
  };
  return made;
}

function fakeSelection(text: string, ranges: ReturnType<typeof range>[]): Selection {
  return {
    rangeCount: ranges.length,
    isCollapsed: text === "",
    toString: () => text,
    getRangeAt: (i: number) => ranges[i],
  } as unknown as Selection;
}

describe("the selection taken when a pointer goes down", () => {
  const word = {};
  const other = {};

  it("is nothing without a range", () => {
    expect(selectionCopy(null, null)).toBeNull();
    expect(selectionCopy(fakeSelection("", []), null)).toBeNull();
  });

  it("keeps its text and copies of its ranges, so a later change of the selection does not count", () => {
    const held = range([word]);
    const copy = selectionCopy(fakeSelection("Hello", [held]), null)!;
    expect(held.cloned).toBe(1);
    expect(copy.text).toBe("Hello");
    expect(copy.holds(word as never)).toBe(true);
    expect(copy.holds(other as never)).toBe(false);
  });

  it("holds the point under the pointer, not just the element around it", () => {
    const text = {};
    const copy = selectionCopy(fakeSelection("Hello", [range([word], [[text, 3]])]), { node: text as Node, offset: 3 })!;
    expect(copy.holds(word as never)).toBe(true);
    const outside = selectionCopy(fakeSelection("Hello", [range([word], [[text, 3]])]), { node: text as Node, offset: 9 })!;
    // The click is in the same block, but not on the selected words.
    expect(outside.holds(word as never)).toBe(false);
  });
});

describe("the window's menu guard", () => {
  function event(target: MenuTarget | null) {
    return { target, clientX: 1, clientY: 2, preventDefault: vi.fn() };
  }

  it("uses the selection from when the pointer went down", () => {
    const taken: MenuSelection[] = [selection("Hello", true), selection("word", true)];
    const take = vi.fn(() => taken.shift() ?? null);
    const guard = new WebMenuGuard(take);
    guard.pressed({ clientX: 1, clientY: 2 });
    const menu = event(element());
    guard.menu(menu);
    expect(menu.preventDefault).not.toHaveBeenCalled();
    expect(take).toHaveBeenCalledTimes(1);
  });

  it("drops the menu when no text was selected then, whatever WebKit selected since", () => {
    const take = vi.fn().mockReturnValueOnce(null).mockReturnValue(selection("word", true));
    const guard = new WebMenuGuard(take);
    guard.pressed({ clientX: 1, clientY: 2 });
    const menu = event(element());
    guard.menu(menu);
    expect(menu.preventDefault).toHaveBeenCalled();
  });

  it("takes the selection at the menu when no pointer went down, and only once per press", () => {
    const take = vi.fn(() => null);
    const guard = new WebMenuGuard(take);
    const first = event(element({ type: "text" }));
    guard.menu(first);
    expect(first.preventDefault).not.toHaveBeenCalled();
    guard.pressed({ clientX: 1, clientY: 2 });
    guard.menu(event(element()));
    guard.menu(event(element()));
    expect(take).toHaveBeenCalledTimes(3);
  });

  it("drops the menu on something that is no element", () => {
    const guard = new WebMenuGuard(() => null);
    const menu = event({} as MenuTarget);
    guard.menu(menu);
    expect(menu.preventDefault).toHaveBeenCalled();
  });
});
