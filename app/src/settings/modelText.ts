// The texts of the OCR model rows (ModelDownload.svelte). The licence lines come from
// `notedown model pull --json --licences-only` (the model_licences command).

import { formatBytes } from "./format";

export const MODEL_TEXT = {
  note: "Qwen3.5-4B reads your notes on your Mac. Your notes never leave it.",
  size: "About 3.6 GB",
};

/** The licence line under the model row; null without lines. */
export function licenceLine(lines: string[]): string | null {
  return lines.length > 0 ? `Licences: ${lines.join("; ")}` : null;
}

/** Download starts only while the licences are on screen. */
export function canDownload(licences: { lines: string[] | null; error: string | null }): boolean {
  return licenceLine(licences.lines ?? []) !== null;
}

/** The button that deletes the model files of Notedown 0.1, with the bytes they take. Empty
 * leftovers (a part file with no bytes yet) get no size. */
export function cleanOldButton(bytes: number): string {
  return bytes > 0 ? `Remove the old model files (${formatBytes(bytes)})` : "Remove the old model files";
}

/** The question before the old model files are deleted; it names the size when there is one. */
export function cleanOldQuestion(bytes: number): string {
  const frees = bytes > 0 ? ` This frees ${formatBytes(bytes)}.` : "";
  return `Delete the old model files of Notedown 0.1?${frees} The new model stays.`;
}

/** The names of the model files in the folder. */
export function filesNote(files: string[]): string {
  return files.join(", ");
}
