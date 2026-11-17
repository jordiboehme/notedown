import { dragTo, pressRow, release, type Drag, type Dragged, type Point } from "./actions";

// A row dragged onto a folder with the pointer (no HTML5 drag and drop: the web view hands
// file drops to Rust). The rows start it on a press; the window follows the pointer and makes
// the move when it is let go. The view models in actions.ts decide.

export const drag = $state<{ current: Drag | null; at: Point }>({ current: null, at: { x: 0, y: 0 } });

/** The folder under a point in CSS pixels: the nearest `[data-folder]` there. */
export function folderUnder(x: number, y: number): string | null {
  const found = document.elementFromPoint(x, y)?.closest<HTMLElement>("[data-folder]");
  return found?.dataset.folder ?? null;
}

/** A press on a row that can be moved: a drag once the pointer moves far enough. */
export function pressOn(dragged: Dragged, event: PointerEvent) {
  if (event.button !== 0 || event.ctrlKey || event.metaKey) return;
  drag.current = pressRow(dragged, { x: event.clientX, y: event.clientY });
  drag.at = { x: event.clientX, y: event.clientY };
}

/** The pointer moved: the folder under it (client coordinates are CSS pixels already). */
export function moveTo(event: PointerEvent) {
  if (!drag.current) return;
  // Let go outside the window: no pointerup came, so no drag may go on without a button.
  if (event.buttons === 0) {
    stopDrag();
    return;
  }
  drag.at = { x: event.clientX, y: event.clientY };
  drag.current = dragTo(drag.current, drag.at, folderUnder(event.clientX, event.clientY));
}

/**
 * The pointer was let go: the move to make, if any. A drag swallows the click that follows,
 * so the row under the pointer is not picked or opened.
 */
export function letGo(): { id: number; toFolder: string } | null {
  const current = drag.current;
  drag.current = null;
  if (!current) return null;
  if (current.active) swallowClick();
  return release(current);
}

/** Stops a drag (Escape, or the pointer left the window). */
export function stopDrag() {
  if (drag.current?.active) swallowClick();
  drag.current = null;
}

function swallowClick() {
  const stop = (event: Event) => {
    event.stopPropagation();
    event.preventDefault();
  };
  window.addEventListener("click", stop, { capture: true, once: true });
  setTimeout(() => window.removeEventListener("click", stop, { capture: true }), 0);
}
