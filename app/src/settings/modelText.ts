// The texts of the OCR model rows (ModelDownload.svelte). The licence lines come from
// `notedown model pull --json --licences-only` (the model_licences command).

export const MODEL_TEXT = {
  note: "Qwen3.5-4B reads your notes on your Mac. Your notes never leave it.",
  size: "About 3.6 GB",
  /** The button that deletes the model files of Notedown 0.1. */
  cleanOld: "Remove the old model files (3.4 GB)",
};

/** The licence line under the model row; null without lines. */
export function licenceLine(lines: string[]): string | null {
  return lines.length > 0 ? `Licences: ${lines.join("; ")}` : null;
}
