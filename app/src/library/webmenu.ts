// WebKit's own context menu in the library window (Open Image in New Window, Download Image,
// Reload and the rest) does not belong in an app. It stays only where it does work: in text
// fields (cut, copy and paste) and on selected text (copy). Everywhere else the window drops it;
// the rows and the pages open their own menus (Rust's).

/** What the menu was opened on: the parts of an element the check reads. */
export type MenuTarget = { closest(selectors: string): unknown };

/** The selection of the window: the parts the check reads. */
export type MenuSelection = {
  readonly isCollapsed: boolean;
  toString(): string;
  containsNode(node: never, allowPartial?: boolean): boolean;
};

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
  if (!selection || selection.isCollapsed || selection.toString().trim() === "") return false;
  return selection.containsNode(target as never, true);
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

/**
 * The selection when a pointer went down: kept until the menu event that may follow, so a word
 * WebKit selects for its menu does not count as selected text.
 */
export function selectionCopy(selection: Selection | null): MenuSelection | null {
  if (!selection || selection.rangeCount === 0) return null;
  const ranges = Array.from({ length: selection.rangeCount }, (_, i) => selection.getRangeAt(i).cloneRange());
  const text = selection.toString();
  return {
    isCollapsed: selection.isCollapsed,
    toString: () => text,
    containsNode: (node: never) => ranges.some((range) => range.intersectsNode(node)),
  };
}
