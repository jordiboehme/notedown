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

/**
 * The answer of `library_list`: the folder as loaded so far (it replaces what the window shows)
 * and the cursor of its next page, or null once it is complete.
 */
export type Listing = { folder: FolderState; next: string | null };

/**
 * A listing the window shows, for `library_shown(listings, binShown)`: the library keeps and
 * follows only these, and lists only these again after a gap or a long pause. While `binShown`
 * is true it follows the events even without a listing, so a delete from elsewhere sends
 * `recycle`.
 */
export type ShownListing = { path: string; foldersOnly: boolean };

export type RecycleItem = {
  key: string;
  path: string;
  name: string;
  kind: "file" | "folder";
  deletedAt: number;
  size: number;
  files: number | null;
};

/** The answer of `library_recycle`: one page of the bin. */
export type RecyclePage = { items: RecycleItem[]; next: string | null };

export type LibraryState = "loggedOut" | "notSetUp" | "unreachable" | "notImported" | "refused" | "ready";

/**
 * The body of a `state` event, and the answer of `library_state` (which is null before the
 * first binding). `page` is the Settings page its button opens.
 */
export type LibraryStateView = { state: LibraryState; text: string | null; page: string | null };

export type Clash = { name: string; existing: string; canReplace: boolean };

/**
 * An upload, a download or a change. The change commands (`library_new_folder`, `library_rename`,
 * `library_move`, `library_delete`, `library_restore`, `library_empty`) answer their job as it
 * ended. A restore into a taken path answers it as `asking` with `question`; the window asks for
 * another name and calls `library_restore(key, path)`, which goes on with the same job.
 * `library_download(id)` answers its `download` job as it was queued (label the file name, `total`
 * its size); its events follow, and a done job's `result` is the absolute path of the file in the
 * Downloads folder (`name (2).ext` when the name is taken). A failed download keeps what came:
 * `library_retry(job)` goes on from there, `library_cancel(job)` ends it. `library_reveal(path)`
 * shows that `result` in the Finder; it takes only a path a download of this run answered.
 */
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
  /** The drop an upload came from (for Cancel all of the drop); null for other jobs. */
  dropId: number | null;
};

/**
 * A binding sends `state` and then `relist` (drop what is shown and list again); the answer of a
 * call sends `state` alone. `folder` carries a listing of the folder: the tree's (`foldersOnly`,
 * only its child folders) or the list's; the window may show the same path in both. `next` is
 * the cursor of its next page (null once complete): a folder Rust listed again by itself after
 * a gap or a long pause pages on from it. `recycle` says the bin may have changed (the app's own delete,
 * restore or Empty, or a delete the library read from the server): the window lists the bin
 * again when it shows it, and ignores it otherwise. `drop` says files or folders were dropped on
 * the window: Rust keeps their paths under `dropId`; the window finds the folder at `position`
 * (physical pixels: divide by `devicePixelRatio`) and calls `library_upload(dropId, folder)`.
 */
export type LibraryEvent =
  | { type: "state"; state: LibraryState; text: string | null; page: string | null }
  | { type: "folder"; folder: FolderState; foldersOnly: boolean; next: string | null }
  | { type: "job"; job: Job }
  | { type: "relist" }
  | { type: "recycle" }
  | { type: "drop"; dropId: number; position: { x: number; y: number }; count: number };

/**
 * What `library_upload(dropId, folder, many?, packages?)` asks before it starts (null: the jobs
 * are queued). `packages`: the drop holds packages such as an `.app`; call again with
 * `packages: true` to upload what is in them or `false` to leave them out. `many`: more than 500
 * files ("Upload 1,234 files?"); call again with `many: true`. Packages are asked first.
 */
export type UploadAsk = { ask: "many" | "packages"; text: string };

/**
 * The answer to an upload job that asks (`library_answer(job, choice)`). `replace` only when its
 * question says `canReplace`. Other upload commands: `library_retry(job)` for a failed upload,
 * `library_cancel(job)` (also ends a restore that asks), `library_cancel_drop(dropId)` (Cancel all).
 */
export type UploadChoice = "replace" | "keepBoth" | "skip";

/** The name of the Tauri event that carries every `LibraryEvent`. */
export const LIBRARY_EVENT = "library";

/**
 * A note of the pipeline's store (src-tauri/src/notes.rs), as `notes_new` and `notes_all` answer
 * it. `changedMs` is the time its store file last changed.
 */
export type NoteSummary = {
  key: string;
  path: string;
  title: string;
  pages: number;
  pagesWithText: number;
  changedMs: number;
};

/**
 * The name of the Tauri event that tells the panel and the library that the notes changed. It
 * carries nothing: ask `notes_new` or `notes_all` again.
 */
export const NOTES_EVENT = "notes";

/**
 * A note for the review (src-tauri/src/review.rs), as `note_open(path)` answers it: the Markdown
 * of each page from the pipeline's store. Show it as text (`white-space: pre-wrap`), never as
 * HTML. `catchingUp`: a page waits for OCR, or the server has a newer version of the note.
 * `landscape` is always false for now (the store has no orientation): take the aspect ratio of a
 * page from its loaded image (Task 13). A note the pipeline has not read yet answers "The text of
 * this note is not ready yet.", a gone one "This note is not there anymore.".
 *
 * `note_page_image(path, index)` answers page `index` as a `data:image/png;base64,...` URL
 * ("This page is not in the note." past its pages). At most two pages render at once, the
 * newest request first: ask only for pages near the view. When a page goes far away, drop its
 * URL and call `note_page_drop(path, index)`: its waiting call fails with "The page is not shown
 * anymore." and its render stops. `note_close(path)` ends the review: its waiting calls fail
 * with "The review was closed." and its renders stop. Hiding the library window closes every
 * review.
 */
export type NoteView = {
  key: string;
  path: string;
  title: string;
  catchingUp: boolean;
  pages: {
    index: number;
    state: "done" | "pending" | "failed" | "truncated";
    markdown: string;
    landscape: boolean;
  }[];
};

/**
 * Where Rust opens the library window (`open_library(note?, newNotes?)`): at the review of a
 * note (its path), or at New. A new window finds it in `window.__NOTEDOWN_AT__` (null: as it
 * starts), an open one gets it as the `LIBRARY_AT_EVENT`.
 */
export type LibraryAt = { at: "note"; path: string } | { at: "new" };

/** The name of the Tauri event that tells an open library window where to go. */
export const LIBRARY_AT_EVENT = "library-at";
