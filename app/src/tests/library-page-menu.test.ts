import { beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { pageMenu } from "../library/review";
import { keepsWebMenu, type MenuSelection, type MenuTarget } from "../library/webmenu";

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
  return { isCollapsed: text === "", toString: () => text, containsNode: () => holds };
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
