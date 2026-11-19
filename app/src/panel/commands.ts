/** Sends a command to Rust; the panel window passes its `invoke` wrapper. */
export type Send = (command: string, args?: Record<string, unknown>) => void;

/**
 * The OCR row's pause calls: the Pause OCR menu at `x`, `y` (CSS pixels of the window, under the
 * button) and Resume OCR Now. Rust registers both and grants them to the panel only.
 */
export function pauseCalls(send: Send) {
  return {
    pauseMenu: (x: number, y: number) => send("panel_pause_menu", { x, y }),
    resumeOcr: () => send("panel_ocr_resume"),
  };
}
