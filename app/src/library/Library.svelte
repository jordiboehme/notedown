<script lang="ts">
  import { onMount, untrack } from "svelte";
  import { SvelteSet } from "svelte/reactivity";
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import {
    answerRestore,
    answerUpload,
    cancelRestore,
    chooseView,
    clearJobs,
    deleteRow,
    askEmpty,
    confirmEmpty,
    downloadEntry,
    dropEmpty,
    entryOf,
    isBusy,
    jobAction,
    library,
    listBin,
    listBinNext,
    listFolder,
    listNext,
    loadView,
    makeFolder,
    moveRow,
    renameRow,
    restoreDraft,
    restoreFocused,
    restoreItem,
    retryJobs,
    setDropTarget,
    showListings,
    startLibrary,
  } from "../lib/library.svelte";
  import { LIBRARY_AT_EVENT, MENU_ERROR_EVENT, NOTES_EVENT, type Entry, type NoteSummary } from "../lib/library-types";
  import { startStatus, status } from "../lib/status.svelte";
  import ColumnsView from "./ColumnsView.svelte";
  import FileActions from "./FileActions.svelte";
  import FolderList from "./FolderList.svelte";
  import Jobs from "./Jobs.svelte";
  import NotesList from "./NotesList.svelte";
  import RecycleBin from "./RecycleBin.svelte";
  import Review from "./Review.svelte";
  import SearchResults from "./SearchResults.svelte";
  import Sidebar from "./Sidebar.svelte";
  import StateNotice from "./StateNotice.svelte";
  import StatusLine from "./StatusLine.svelte";
  import Toolbar from "./Toolbar.svelte";
  import {
    columnsView,
    listingKey,
    openAt,
    parentOf,
    pathBar,
    rootRows,
    shownFor,
    sidebarItems,
    treeView,
    type Selection,
    type SidebarItem,
    type Sort,
  } from "./view";
  import { entryActions, folderAt, followChange, jobRows, type Change } from "./actions";
  import { emptyConfirm, recycleRows } from "./recycle";
  import { drag, folderUnder, letGo, moveTo, stopDrag } from "./drag.svelte";
  import { isFindKey, searchPlace, type ResultRow } from "./search";
  import { LibrarySearch } from "./search.svelte";
  import "../lib/system.css";
  import "./library.css";

  // The library window: the status line, the sidebar with the notes and the device trees, and
  // the selected folder or notes, or the review of a note. It wires the store to Rust; the view
  // models decide. Rust may open it at a note or at New (`__NOTEDOWN_AT__`, `library-at`).
  // What it shows and where Back in the review goes are in the store: a new binding leaves the
  // review there.
  library.selection = openAt((window as unknown as { __NOTEDOWN_AT__?: unknown }).__NOTEDOWN_AT__) ?? { kind: "new" };
  library.back = { kind: "new" };
  /**
   * Counts the openings of a note: the review is keyed by it, so a note opened again (from the
   * panel, while the hidden window still shows it) is read again and marked seen.
   */
  let openings = $state(0);
  const open = new SvelteSet<string>();
  let sort = $state<Sort>({ by: "name", ascending: true });
  let newNotes = $state<NoteSummary[] | null>(null);
  let allNotes = $state<NoteSummary[] | null>(null);
  /** The row picked for the file actions: a row of the list, or a file picked in a column. */
  let picked = $state<string | null>(null);
  /** The row whose name is being typed. */
  let renaming = $state<string | null>(null);
  /** The name of a new folder is being typed. */
  let naming = $state(false);
  let now = $state(Date.now());
  /** The search of the toolbar; while it has text, the main area shows its results. */
  const search = new LibrarySearch(() => searchPlace(library.selection, newNotes, allNotes));
  const searching = $derived(search.text.trim() !== "");
  let toolbar: Toolbar | undefined = $state();
  /** The clock of the job rows: done rows go after a while. */
  let tick = $state(Date.now());

  const ready = $derived(library.view?.state === "ready");
  const selectedPath = $derived(library.selection.kind === "folder" ? library.selection.path : null);
  const tree = $derived(treeView(library.listings, open, selectedPath));
  const columns = $derived(columnsView(selectedPath, library.listings, picked));
  const inTree = $derived(library.viewMode === "tree");
  const items = $derived(
    sidebarItems(library.view, { newNotes: newNotes?.length ?? null, allNotes: allNotes?.length ?? null }, library.selection),
  );

  // The file actions work on the picked row; in the columns, without one, on the shown folder.
  const actedOn = $derived(picked ?? (inTree ? null : selectedPath));
  const actions = $derived(entryActions(entryOf(actedOn), selectedPath));
  const groups = $derived(jobRows(library.jobs, library.ended, tick, library.dropTotals));
  const binRows = $derived(recycleRows(library.bin.items, now));

  function report(command: string) {
    return (error: unknown) => console.error(`notedown: ${command} failed`, error);
  }

  // Rust follows only what the window shows: tell it on every change, and again after a new
  // binding (the generation moves).
  $effect(() => {
    void library.generation;
    const shown = shownFor(library.viewMode, library.selection, open);
    const binShown = library.selection.kind === "bin";
    untrack(() => showListings(shown, binShown));
  });

  // The bin lists from page 1 when it is shown, on a `recycle` event while it is shown, and
  // after a new binding.
  $effect(() => {
    void library.recycle;
    void library.generation;
    if (!ready || library.selection.kind !== "bin") return;
    untrack(() => void listBin());
  });

  // The tree lists a folder when it opens, and the rest of its pages after the first.
  $effect(() => {
    if (!ready || !inTree) return;
    const { missing, more } = tree;
    untrack(() => {
      for (const path of missing) void listFolder(path, true);
      for (const path of more) void listNext(path, true);
    });
  });

  // The selected folder: its first page at once (again after a new binding).
  $effect(() => {
    if (!ready || !inTree || library.selection.kind !== "folder") return;
    const path = library.selection.path;
    if (library.listings[listingKey(path, false)] === undefined) untrack(() => void listFolder(path, false));
  });

  // The columns list a folder when they show it; the next pages come as a column scrolls.
  $effect(() => {
    if (!ready || inTree) return;
    const { missing } = columns;
    untrack(() => {
      for (const item of missing) void listFolder(item.path, item.foldersOnly);
    });
  });

  // "This folder" follows the shown place: a search that is on runs again for it.
  $effect(() => {
    const selection = library.selection;
    void (selection.kind === "folder" || selection.kind === "note" ? selection.path : selection.kind);
    untrack(() => search.rerun());
  });

  function loadNotes() {
    invoke<NoteSummary[]>("notes_new")
      .then((notes) => (newNotes = notes))
      .catch(report("notes_new"));
    invoke<NoteSummary[]>("notes_all")
      .then((notes) => (allNotes = notes))
      .catch(report("notes_all"));
  }

  onMount(() => {
    // A drop goes into the folder under it (the position is physical pixels of the web
    // view); the shown folder's listing carries its path, so a drop there goes into it.
    setDropTarget((position) => folderAt(position, window.devicePixelRatio, folderUnder));
    const stops = [
      startLibrary(),
      startStatus(),
      listen(NOTES_EVENT, loadNotes),
      listen<string>(MENU_ERROR_EVENT, (event) => (library.notice = String(event.payload))),
      listen(LIBRARY_AT_EVENT, (event) => {
        const at = openAt(event.payload);
        if (at?.kind === "note") openNote(at.path);
        else if (at) show(at);
      }),
    ];
    loadNotes();
    void loadView();
    const clock = setInterval(() => (now = Date.now()), 30_000);
    const jobsClock = setInterval(() => {
      tick = Date.now();
      clearJobs(tick);
    }, 1000);
    return () => {
      setDropTarget(null);
      clearInterval(clock);
      clearInterval(jobsClock);
      for (const stop of stops) void stop.then((unlisten) => unlisten());
    };
  });

  /** Shows `next`: the picked row, a rename and a new folder's name end. */
  function show(next: Selection) {
    library.selection = next;
    picked = null;
    renaming = null;
    naming = false;
  }

  /** Shows the review of the note at `path`; Back goes to what was shown before. */
  function openNote(path: string) {
    if (library.selection.kind !== "note") library.back = library.selection;
    show({ kind: "note", path });
    openings += 1;
  }

  /** Shows a folder and opens the folders above it in the tree. */
  function select(path: string) {
    library.selection = { kind: "folder", path };
    picked = null;
    renaming = null;
    naming = false;
    for (const crumb of pathBar(path).slice(0, -1)) open.add(crumb.path);
  }

  /** A file picked in a column: the columns right of it close, and the file is marked. */
  function pickFile(file: string) {
    select(parentOf(file));
    picked = file;
  }

  /** After a change, the shown folder and the picked row follow it. */
  function follow(change: Change | null) {
    if (!change) return;
    const next = followChange({ shown: selectedPath, picked }, change);
    if (next.shown !== null && next.shown !== selectedPath) select(next.shown);
    picked = next.picked;
  }

  function rename(entry: Entry, typed: string | null) {
    renaming = null;
    void renameRow(entry, typed).then(follow);
  }

  function named(typed: string | null) {
    naming = false;
    void makeFolder(selectedPath, typed);
  }

  async function remove() {
    const entry = entryOf(actedOn);
    if (entry) follow(await deleteRow(entry));
  }

  function download() {
    const entry = entryOf(actedOn);
    if (entry) void downloadEntry(entry.id);
  }

  function released() {
    const wanted = letGo();
    if (wanted) void moveRow(wanted.id, wanted.from, wanted.toFolder).then(follow);
  }

  function toggle(path: string) {
    if (open.has(path)) {
      // Closing a folder closes the folders below it too: they are not shown.
      for (const other of [...open]) if (other === path || other.startsWith(`${path}/`)) open.delete(other);
    } else {
      open.add(path);
    }
  }

  function pick(id: SidebarItem["id"]) {
    library.selection = { kind: id };
  }

  // The sidebar's New row and the New list's button: the menu is Rust's, the button calls the
  // same command. The lists reload when the notes change.
  function newMenu() {
    invoke("library_menu", { target: { kind: "newNotes" } }).catch(report("library_menu"));
  }

  function markAllSeen() {
    invoke<number>("notes_seen_all").then(loadNotes).catch((error) => (library.notice = String(error)));
  }

  /** A row of the search results: it opens or selects what it names, and the search ends. */
  function openRow(row: ResultRow) {
    search.clear();
    if (row.kind === "folder") select(row.path);
    else if (row.kind === "note") openNote(row.path);
    else pickFile(row.path);
  }

  function keydown(event: KeyboardEvent) {
    if (event.key === "Escape" && drag.current) stopDrag();
    if (isFindKey(event)) {
      event.preventDefault();
      toolbar?.focusSearch();
    }
  }

  function settings(page: string | null) {
    invoke("open_settings", { page }).catch(report("open_settings"));
  }
</script>

<svelte:window
  onfocus={() => (now = Date.now())}
  onblur={stopDrag}
  onpointermove={moveTo}
  onpointerup={released}
  onpointercancel={stopDrag}
  onkeydown={keydown}
/>

<div class="library window">
  <StatusLine status={status.current} {now} onSetUp={() => settings("setup")} />
  <Toolbar bind:this={toolbar} view={library.viewMode} onView={(view) => void chooseView(view)} {search} />
  <div class="body">
    <Sidebar
      notes={items.notes}
      tree={items.tree}
      rows={inTree ? tree.rows : rootRows(selectedPath)}
      bin={items.bin}
      view={library.view}
      onPick={pick}
      onSelect={select}
      onToggle={toggle}
      onSettings={settings}
      onNewMenu={newMenu}
    />
    <main class="main">
      {#if searching}
        <SearchResults {search} onRow={openRow} />
      {:else if library.selection.kind === "new"}
        <NotesList
          title="New notes"
          notes={newNotes}
          empty="No new notes."
          onOpen={(note) => openNote(note.path)}
          onMarkAllSeen={markAllSeen}
        />
      {:else if library.selection.kind === "all"}
        <NotesList title="All notes" notes={allNotes} empty="No notes with text yet." onOpen={(note) => openNote(note.path)} />
      {:else if library.selection.kind === "note"}
        {#key `${library.generation}:${openings}:${library.selection.path}`}
          <Review path={library.selection.path} onBack={() => (library.selection = library.back)} />
        {/key}
      {:else if !ready}
        <StateNotice view={library.view} onSettings={settings} />
      {:else if library.selection.kind === "bin"}
        <RecycleBin
          bin={library.bin}
          rows={binRows}
          restores={library.restores}
          question={emptyConfirm(library.bin, library.emptyAsked)}
          emptying={library.emptying}
          onMore={() => void listBinNext()}
          onRestore={(item) => void restoreItem(item)}
          onAnswer={(key, typed) => void answerRestore(key, typed)}
          onCancel={(key) => void cancelRestore(key)}
          onDraft={restoreDraft}
          onFocused={restoreFocused}
          onAskEmpty={askEmpty}
          onDropEmpty={dropEmpty}
          onEmpty={() => void confirmEmpty()}
        />
      {:else}
        {@const path = library.selection.path}
        <FileActions
          {actions}
          {naming}
          onNewFolder={() => (naming = true)}
          onNamed={named}
          onRename={() => (renaming = actedOn)}
          onDelete={() => void remove()}
          onDownload={download}
          onOpen={() => actedOn && openNote(actedOn)}
        />
        {#if !inTree}
          {#key library.generation}
            <ColumnsView
              {path}
              columns={columns.columns}
              {renaming}
              onSelect={select}
              onFile={pickFile}
              onOpenNote={openNote}
              onRename={rename}
            />
          {/key}
        {:else}
          {#key `${path}:${library.generation}`}
            <FolderList
              {path}
              folder={library.listings[listingKey(path, false)]}
              next={library.next[listingKey(path, false)] ?? null}
              busy={isBusy(path, false)}
              {sort}
              {picked}
              {renaming}
              onSort={(next) => (sort = next)}
              onOpen={select}
              onOpenNote={openNote}
              onMore={() => void listNext(path, false)}
              onPick={(row) => (picked = row)}
              onRename={rename}
            />
          {/key}
        {/if}
      {/if}
    </main>
  </div>
  <Jobs
    {groups}
    asks={library.asks}
    notice={library.notice}
    onAction={(job, action) => void jobAction(job, action)}
    onRetryAll={(ids) => void retryJobs(ids)}
    onAnswer={(dropId, choice) => void answerUpload(dropId, choice)}
    onNoticeSeen={() => (library.notice = null)}
  />
  {#if drag.current?.active}
    <div class="ghost" style:left="{drag.at.x + 12}px" style:top="{drag.at.y + 8}px" aria-hidden="true">
      {drag.current.dragged.path.slice(drag.current.dragged.path.lastIndexOf("/") + 1)}
    </div>
  {/if}
</div>

<style>
  .window {
    display: flex;
    flex-direction: column;
    height: 100%;
  }

  .body {
    flex: 1;
    display: flex;
    min-height: 0;
  }

  .main {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-width: 0;
    background: var(--box);
  }

  /* The name of a dragged row next to the pointer; it lets the pointer through. */
  .ghost {
    position: fixed;
    z-index: 10;
    max-width: 240px;
    padding: 2px 8px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    pointer-events: none;
    color: var(--on-brand);
    background: var(--brand);
    border-radius: 5px;
    opacity: 0.9;
  }
</style>
