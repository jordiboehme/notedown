import type { NoteSummary } from "../lib/library-types";
import { folderLabel } from "../library/view";

/** How many new notes the panel lists; the rest is "N more". */
export const PANEL_NOTES = 5;

export type PanelNote = { key: string; path: string; title: string; folder: string };

/**
 * The new notes of the panel: the first five (newest first, as `notes_new` answers them) and
 * "N more" for the rest. Nothing before the notes are read.
 */
export function newNotesView(notes: NoteSummary[] | null): { rows: PanelNote[]; more: string | null } {
  if (!notes) return { rows: [], more: null };
  const rows = notes.slice(0, PANEL_NOTES).map((n) => ({ key: n.key, path: n.path, title: n.title, folder: folderLabel(n.path) }));
  const rest = notes.length - rows.length;
  return { rows, more: rest > 0 ? `${rest.toLocaleString("en-US")} more` : null };
}
