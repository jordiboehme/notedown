import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { Status } from "../lib/api";
import { needsQuitWarning } from "./lines";

/** The event with which Rust asks the panel to put the question (tray Quit, Cmd+Q). */
export const CONFIRM_QUIT_EVENT = "confirm-quit";

export const QUIT_QUESTION = "The Supernote cannot sync while Notedown is not running. Quit anyway?";

/** Whether the panel shows the Quit question now. */
export const quitQuestion = $state({ open: false });

/** Opens the question whenever Rust asks for it. Returns the function that stops following. */
export async function followQuitRequests(): Promise<() => void> {
  return listen(CONFIRM_QUIT_EVENT, () => {
    quitQuestion.open = true;
  });
}

/**
 * The panel's Quit: asks first when the status says so; otherwise Rust decides with its own,
 * newer view of the server (it asks for the question through the event, or quits).
 */
export function pressQuit(status: Status | null): void {
  if (needsQuitWarning(status)) {
    quitQuestion.open = true;
    return;
  }
  void invoke("request_quit").catch((error: unknown) => console.error("notedown: Quit failed", error));
}

/** Quit in the question: quits without asking again. */
export function confirmQuit(): void {
  quitQuestion.open = false;
  void invoke("quit_app").catch((error: unknown) => console.error("notedown: Quit failed", error));
}

/** Cancel in the question, or the panel lost the focus. */
export function cancelQuit(): void {
  quitQuestion.open = false;
}
