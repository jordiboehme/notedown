// The texts of the OCR model rows (ModelDownload.svelte). The licence lines come from
// `notedown model pull --json --licences-only` (the model_licences command).

import { formatBytes } from "./format";
import type { LicenceFile } from "./api";

export const MODEL_TEXT = {
  note: "Qwen3.5-4B reads your notes on your Mac. Your notes never leave it.",
  size: "About 3.6 GB",
};

/** A model of a licence group, linked to its page on Hugging Face (null for any other page). */
export type LicensedModel = { name: string; page: string | null };

/** One part of the licence line: the models under one licensor and licence. */
export type LicenceGroup = { models: LicensedModel[]; licensor: string; licence: string };

const HUGGING_FACE = /^https:\/\/huggingface\.co\/[A-Za-z0-9._-]+(\/[A-Za-z0-9._-]+)*$/;

/**
 * The model files grouped by licensor and licence, in their order, each model once: the Qwen
 * model and its mmproj are one model, the two detectors share one group. The notedown program
 * groups its licence lines the same way (crates/notedown-ocr/src/model.rs, `licence_lines`): a
 * change here changes it there too.
 */
export function licenceGroups(files: LicenceFile[]): LicenceGroup[] {
  const groups: LicenceGroup[] = [];
  for (const file of files) {
    let group = groups.find((g) => g.licensor === file.licensor && g.licence === file.licence);
    if (!group) {
      group = { models: [], licensor: file.licensor, licence: file.licence };
      groups.push(group);
    }
    if (!group.models.some((m) => m.name === file.model)) {
      group.models.push({ name: file.model, page: HUGGING_FACE.test(file.page) ? file.page : null });
    }
  }
  return groups;
}

/** The separators between the models of a group: "A", "A and B", "A, B and C". */
export function modelJoint(index: number, count: number): string {
  if (index === 0) return "";
  return index === count - 1 ? " and " : ", ";
}

/** The licence line under the model row as text; null without files. */
export function licenceLine(files: LicenceFile[]): string | null {
  const groups = licenceGroups(files);
  if (groups.length === 0) return null;
  const parts = groups.map((group) => {
    const names = group.models.map((m, i) => `${modelJoint(i, group.models.length)}${m.name}`).join("");
    return `${names} (${group.licensor}): ${group.licence}`;
  });
  return `Licences: ${parts.join("; ")}`;
}

/** Download starts only while the licences are on screen. */
export function canDownload(licences: { files: LicenceFile[] | null; error: string | null }): boolean {
  return licenceLine(licences.files ?? []) !== null;
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
