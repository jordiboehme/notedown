import type { ChildState } from "../lib/api";

// The "Markdown folder" row of the OCR page: the folder of the logged-in account, Choose...
// (the folder picker) and Show in Finder.

export interface MarkdownFolder {
  /** The folder; null without an account; undefined while it is not read yet. */
  path: string | null | undefined;
  /** Why the last choice or Show in Finder failed. */
  error: string | null;
  /** A choice is being saved. */
  busy: boolean;
  /** The pipeline's state, from the status; null while there is none. */
  pipeline: ChildState | null;
}

export interface MarkdownRow {
  title: string;
  explanation: string;
  path: string | null;
  note: string | null;
  tone: "secondary" | "bad";
  choose: { label: string; enabled: boolean };
  show: { label: string; enabled: boolean };
}

export const NEEDS_LOGIN = "Log in to choose the Markdown folder.";

/** The pipeline's own refusal of its `--markdown-dir`, without its prefixes; null for others. */
function pipelineRefusal(state: ChildState | null): string | null {
  if (state?.state !== "backoff" || !state.lastError.includes("--markdown-dir")) return null;
  const text = state.lastError
    .replace(/^error:\s*/, "")
    .replace(/^invalid --markdown-dir:\s*/, "");
  return `The pipeline refused this folder: ${text}`;
}

export function markdownRow(folder: MarkdownFolder): MarkdownRow {
  const path = folder.path ?? null;
  const ready = path !== null && !folder.busy;
  let note: string | null = null;
  let tone: MarkdownRow["tone"] = "secondary";
  if (folder.error !== null) {
    note = folder.error;
    tone = "bad";
  } else if (folder.path === undefined) note = "Checking...";
  else if (folder.path === null) note = NEEDS_LOGIN;
  else {
    note = pipelineRefusal(folder.pipeline);
    if (note !== null) tone = "bad";
  }
  return {
    title: "Markdown folder",
    explanation: "Notedown writes the Markdown of your notes here.",
    path,
    note,
    tone,
    choose: { label: "Choose...", enabled: ready },
    show: { label: "Show in Finder", enabled: ready },
  };
}
