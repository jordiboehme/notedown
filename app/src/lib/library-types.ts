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
 * and the cursor of its next page, or null once it is complete. `revision` grows with every
 * change Rust sends (folder events carry it too): the window keeps the folder of the higher one,
 * so an answer that comes after a newer event is dropped.
 */
export type Listing = { folder: FolderState; next: string | null; revision: number };

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
 * A binding sends `state` and then `relist` (drop what is shown and list again; with `leave`
 * the account changed and the window leaves the review); the answer of a call sends `state`
 * alone. `folder` carries a listing of the folder: the tree's (`foldersOnly`,
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
  | { type: "folder"; folder: FolderState; foldersOnly: boolean; next: string | null; revision: number }
  | { type: "job"; job: Job }
  | { type: "relist"; leave: boolean }
  | { type: "recycle" }
  | { type: "drop"; dropId: number; position: { x: number; y: number }; count: number };

/**
 * What `library_upload(dropId, folder, many?, packages?)` asks before it starts (null: the jobs
 * are queued). `packages`: the drop holds packages such as an `.app`; call again with
 * `packages: true` to upload what is in them or `false` to leave them out. `many`: more than 500
 * files ("Upload 1,234 files?"); call again with `many: true`. Packages are asked first.
 */
export type UploadAsk = { ask: "many" | "packages"; text: string };

// An upload job that asks is answered with `library_answer(job, choice)`, `choice` one of
// "replace" (only when its question says `canReplace`), "keepBoth" or "skip". Other upload
// commands: `library_retry(job)` for a failed upload, `library_cancel(job)` (also ends a restore
// that asks), `library_cancel_drop(dropId)` (Cancel all).

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
 * The name of the Tauri event that tells the panel and the library that the notes changed: ask
 * `notes_new` or `notes_all` again. It carries `NotesChanged`.
 */
export const NOTES_EVENT = "notes";

/** A context menu item failed; the payload is the error text, which the window shows as its notice. */
export const MENU_ERROR_EVENT = "library-menu-error";

/**
 * What `NOTES_EVENT` carries: the wire paths of the notes whose store file changed or went (a
 * moved note with its old and its new path). Empty when only the lists changed (a note marked
 * seen). An open review reads its note again only when its path is named.
 */
export type NotesChanged = { paths: string[] };

/**
 * A note for the review (src-tauri/src/review.rs), as `note_open(path)` answers it: every page
 * of the note, with the text of the pipeline's store where it has some. Each page has its
 * `pageId` (the store's page id), its `blocks` (id and text, in order) and its `markdown` (the
 * block texts joined by blank lines). Show the text as text (`white-space: pre-wrap`), never as
 * HTML. A page the OCR has not read yet is `pending` with no blocks. `key` is the note's key in
 * the store, null when the store does not know the note yet (no `note_seen` then).
 * `catchingUp`: a page waits for OCR, or the server has a newer version of the note than the
 * store read. `landscape` comes from the file when the store is behind it, else it is false (the
 * store has no orientation): take the aspect ratio of a page from its loaded image. A gone note
 * answers "This note is not there anymore.". `NOTES_EVENT` names the note when the store wrote
 * one of its pages: call `note_open` again to show its text.
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
  key: string | null;
  /** The md5 of the file version the pages are of; a change means new page images. */
  md5: string | null;
  path: string;
  title: string;
  catchingUp: boolean;
  pages: {
    index: number;
    pageId: string;
    state: "done" | "pending" | "failed" | "truncated";
    markdown: string;
    blocks: { id: string; text: string }[];
    landscape: boolean;
  }[];
};

/**
 * The name of the Tauri event that tells an open library window where to go. Rust opens the
 * window (`open_library(note?, newNotes?)`) at `{ at: "note", path }` (the review of a note) or
 * `{ at: "new" }`. A new window finds it in `window.__NOTEDOWN_AT__` (null: as it starts), an
 * open one gets it as this event; `openAt` (library/view.ts) reads it.
 */
export const LIBRARY_AT_EVENT = "library-at";
