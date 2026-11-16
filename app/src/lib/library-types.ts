// The library's data as Rust sends it (src-tauri/src/library.rs, serde camelCase). Every event
// of the library comes as one `LibraryEvent` on the "library" event; none holds the token.

export type Entry = {
  id: number;
  path: string;
  name: string;
  kind: "file" | "folder";
  size: number | null;
  md5: string | null;
  mtimeMs: number;
  writable: boolean;
};

export type FolderState = {
  path: string;
  entries: Entry[];
  complete: boolean;
  loading: boolean;
  error: string | null;
};

export type LibraryState = "loggedOut" | "notSetUp" | "unreachable" | "notImported" | "refused" | "ready";

/**
 * The body of a `state` event, and the answer of `library_state` (which is null before the
 * first binding). `page` is the Settings page its button opens.
 */
export type LibraryStateView = { state: LibraryState; text: string | null; page: string | null };

export type Clash = { name: string; existing: string; canReplace: boolean };

export type Job = {
  id: number;
  kind: "upload" | "download" | "change";
  label: string;
  done: number;
  total: number | null;
  state: "queued" | "running" | "waiting" | "asking" | "done" | "failed" | "cancelled";
  text: string | null;
  question: Clash | null;
  result: string | null;
};

/**
 * A binding sends `state` and then `relist` (drop what is shown and list again); the answer of a
 * call sends `state` alone.
 */
export type LibraryEvent =
  | { type: "state"; state: LibraryState; text: string | null; page: string | null }
  | { type: "folder"; folder: FolderState }
  | { type: "job"; job: Job }
  | { type: "relist" };

/** The name of the Tauri event that carries every `LibraryEvent`. */
export const LIBRARY_EVENT = "library";
