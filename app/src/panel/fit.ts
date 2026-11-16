/** The width of the panel window (tauri.conf.json and windows.rs). */
export const PANEL_WIDTH = 340;

/** The most the panel may grow (tauri.conf.json, maxHeight); it scrolls beyond. */
export const PANEL_MAX_HEIGHT = 520;

/** The least: one line and the footer. */
const PANEL_MIN_HEIGHT = 60;

/** The height of the panel window for content of `content` points. */
export function panelHeight(content: number): number {
  return Math.min(PANEL_MAX_HEIGHT, Math.max(PANEL_MIN_HEIGHT, Math.ceil(content)));
}
