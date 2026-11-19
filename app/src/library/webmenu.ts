// WebKit's own context menu in the library window (Open Image in New Window, Download Image,
// Reload and the rest) does not belong in an app. It stays only where it does work: in text
// fields (cut, copy and paste) and on selected text (copy). Everywhere else the window drops it;
// the rows and the pages open their own menus (Rust's).

/** What the menu was opened on: the parts of an element the check reads. */
export type MenuTarget = { closest(selectors: string): unknown };

/** The selection when the click began: its text, and whether it holds what was clicked. */
export type MenuSelection = { text: string; holds(target: MenuTarget): boolean };

/** Inputs that hold no text to edit. */
const NO_TEXT = new Set(["button", "checkbox", "color", "file", "hidden", "image", "radio", "range", "reset", "submit"]);

/** Whether `target` is in a field the user types text into. */
function inTextField(target: MenuTarget): boolean {
  const field = target.closest("input, textarea, [contenteditable]") as { type?: unknown; isContentEditable?: boolean } | null;
  if (!field) return false;
  if (field.isContentEditable) return true;
  return typeof field.type === "string" && !NO_TEXT.has(field.type);
}

/** Whether `target` is in the selected text (`selection`, taken when the click began). */
function onSelectedText(target: MenuTarget, selection: MenuSelection | null): boolean {
  if (!selection || selection.text.trim() === "") return false;
  return selection.holds(target);
}

/**
 * Whether a right click on `target` keeps WebKit's menu: in a text field, or on selected text.
 * `selection` is the selection when the click began: WebKit may select the word under the
 * pointer before the menu event.
 */
export function keepsWebMenu(target: MenuTarget | null, selection: MenuSelection | null): boolean {
  if (!target) return false;
  return inTextField(target) || onSelectedText(target, selection);
}

/** A point in the text: what `document.caretRangeFromPoint` gives for the pointer. */
export type TextPoint = { node: Node; offset: number };

/**
 * The selection when a pointer went down at `point`: kept until the menu event that may follow,
 * so a word WebKit selects for its menu does not count as selected text. It holds a click when
 * the point is in a selected range (not merely in the element around it); without a point
 * (an image, no text under the pointer), when the clicked element is in one.
 */
export function selectionCopy(selection: Selection | null, point: TextPoint | null): MenuSelection | null {
  if (!selection || selection.rangeCount === 0) return null;
  const ranges = Array.from({ length: selection.rangeCount }, (_, i) => selection.getRangeAt(i).cloneRange());
  return {
    text: selection.toString(),
    holds: (target) =>
      point
        ? ranges.some((range) => range.isPointInRange(point.node, point.offset))
        : ranges.some((range) => range.intersectsNode(target as unknown as Node)),
  };
}

/**
 * Drops WebKit's menu where it does not belong. `take` reads the selection at a point of the
 * window; it is taken when a pointer goes down (`pressed`) and used by the menu event that
 * follows (`menu`), or taken then when no pointer went down (a menu opened by the keyboard).
 */
export class WebMenuGuard {
  #take: (x: number, y: number) => MenuSelection | null;
  /** The selection of the last press; undefined when the next menu has none. */
  #pressed: MenuSelection | null | undefined = undefined;

  constructor(take: (x: number, y: number) => MenuSelection | null) {
    this.#take = take;
  }

  pressed(event: { clientX: number; clientY: number }) {
    this.#pressed = this.#take(event.clientX, event.clientY);
  }

  menu(event: { target: unknown; clientX: number; clientY: number; preventDefault(): void }) {
    const selection = this.#pressed !== undefined ? this.#pressed : this.#take(event.clientX, event.clientY);
    this.#pressed = undefined;
    const target = event.target as Partial<MenuTarget> | null;
    const element = target && typeof target.closest === "function" ? (target as MenuTarget) : null;
    if (!keepsWebMenu(element, selection)) event.preventDefault();
  }
}
