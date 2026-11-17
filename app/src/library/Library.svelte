<script lang="ts">
  import { onMount, untrack } from "svelte";
  import { SvelteSet } from "svelte/reactivity";
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import {
    answerUpload,
    chooseView,
    clearJobs,
    deleteEntry,
    downloadEntry,
    isBusy,
    jobAction,
    library,
    listFolder,
    listNext,
    loadView,
    moveEntry,
    newFolder,
    renameEntry,
    retryJobs,
    setDropTarget,
    showListings,
    startLibrary,
  } from "../lib/library.svelte";
  import { NOTES_EVENT, type Entry, type NoteSummary } from "../lib/library-types";
  import { startStatus, status } from "../lib/status.svelte";
  import ColumnsView from "./ColumnsView.svelte";
  import FileActions from "./FileActions.svelte";
  import FolderList from "./FolderList.svelte";
  import Jobs from "./Jobs.svelte";
  import NotesList from "./NotesList.svelte";
  import Sidebar from "./Sidebar.svelte";
  import StateNotice from "./StateNotice.svelte";
  import StatusLine from "./StatusLine.svelte";
  import Toolbar from "./Toolbar.svelte";
  import {
    columnsView,
    listingKey,
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
  import { entryActions, entryAt, folderAt, jobRows, newName, shownAfter } from "./actions";
  import { drag, folderUnder, letGo, moveTo, stopDrag } from "./drag.svelte";
  import "../lib/system.css";
  import "./library.css";

  // The library window: the status line, the sidebar with the notes and the device trees, and
  // the selected folder or notes. It wires the store to Rust; the view models decide.
  let selection = $state<Selection>({ kind: "new" });
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
  /** The clock of the job rows: done rows go after a while. */
  let tick = $state(Date.now());

  const ready = $derived(library.view?.state === "ready");
  const selectedPath = $derived(selection.kind === "folder" ? selection.path : null);
  const tree = $derived(treeView(library.listings, open, selectedPath));
  const columns = $derived(columnsView(selectedPath, library.listings, picked));
  const inTree = $derived(library.viewMode === "tree");
  const items = $derived(
    sidebarItems(library.view, { newNotes: newNotes?.length ?? null, allNotes: allNotes?.length ?? null }, selection),
  );

  // The file actions work on the picked row; in the columns, without one, on the shown folder.
  const actedOn = $derived(picked ?? (inTree ? null : selectedPath));
  const actions = $derived(entryActions(actedOn ? entryAt(actedOn, library.listings) : null, selectedPath));
  const groups = $derived(jobRows(library.jobs, library.ended, tick));

  function report(command: string) {
    return (error: unknown) => console.error(`notedown: ${command} failed`, error);
  }

  // Rust follows only what the window shows: tell it on every change, and again after a new
  // binding (the generation moves).
  $effect(() => {
    void library.generation;
    const shown = shownFor(library.viewMode, selection, open);
    untrack(() => showListings(shown));
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
    if (!ready || !inTree || selection.kind !== "folder") return;
    const path = selection.path;
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
    // view), else into the folder that is shown.
    setDropTarget((position) => folderAt(position, window.devicePixelRatio, folderUnder) ?? selectedPath);
    const stops = [startLibrary(), startStatus(), listen(NOTES_EVENT, loadNotes)];
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

  /** Shows a folder and opens the folders above it in the tree. */
  function select(path: string) {
    selection = { kind: "folder", path };
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

  /** After a change of `from` (to `to`, null: deleted): the shown folder follows it. */
  function follow(from: string, to: string | null) {
    if (picked === from) picked = to;
    if (selectedPath !== null) {
      const shown = shownAfter(selectedPath, from, to);
      if (shown !== selectedPath) select(shown);
    }
  }

  async function move(id: number, toFolder: string) {
    const from = entryById(id)?.path;
    const job = await moveEntry(id, toFolder);
    if (from && job?.state === "done" && job.result) follow(from, job.result);
  }

  function entryById(id: number): Entry | null {
    for (const listing of Object.values(library.listings)) {
      const found = listing.entries.find((e) => e.id === id);
      if (found) return found;
    }
    return null;
  }

  async function renamed(entry: Entry, typed: string | null) {
    renaming = null;
    const name = typed === null ? null : newName(typed, entry.name);
    if (name === null) return;
    const job = await renameEntry(entry.id, name);
    if (job?.state === "done" && job.result) follow(entry.path, job.result);
  }

  async function named(typed: string | null) {
    naming = false;
    const parent = selectedPath;
    const name = typed === null ? null : newName(typed, "");
    if (parent === null || name === null) return;
    await newFolder(parent, name);
  }

  function acted(): Entry | null {
    return actedOn ? entryAt(actedOn, library.listings) : null;
  }

  async function remove() {
    const entry = acted();
    if (!entry) return;
    const job = await deleteEntry(entry.id);
    if (job?.state === "done") follow(entry.path, null);
  }

  function download() {
    const entry = acted();
    if (entry) void downloadEntry(entry.id);
  }

  function released() {
    const wanted = letGo();
    if (wanted) void move(wanted.id, wanted.toFolder);
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
    selection = { kind: id };
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
  onkeydown={(event) => event.key === "Escape" && drag.current && stopDrag()}
/>

<div class="library window">
  <StatusLine status={status.current} {now} onSetUp={() => settings("setup")} />
  <Toolbar view={library.viewMode} onView={(view) => void chooseView(view)} />
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
    />
    <main class="main">
      {#if selection.kind === "new"}
        <NotesList title="New notes" notes={newNotes} empty="No new notes." />
      {:else if selection.kind === "all"}
        <NotesList title="All notes" notes={allNotes} empty="No notes with text yet." />
      {:else if !ready}
        <StateNotice view={library.view} onSettings={settings} />
      {:else if selection.kind === "bin"}
        <h1 class="title">Recycle bin</h1>
        <p class="note">The recycle bin is not shown yet.</p>
      {:else}
        {@const path = selection.path}
        <FileActions
          {actions}
          {naming}
          onNewFolder={() => (naming = true)}
          onNamed={(typed) => void named(typed)}
          onRename={() => (renaming = actedOn)}
          onDelete={() => void remove()}
          onDownload={download}
        />
        {#if !inTree}
          {#key library.generation}
            <ColumnsView
              {path}
              columns={columns.columns}
              {renaming}
              onSelect={select}
              onFile={pickFile}
              onRename={(entry, typed) => void renamed(entry, typed)}
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
              onMore={() => void listNext(path, false)}
              onPick={(row) => (picked = row)}
              onRename={(entry, typed) => void renamed(entry, typed)}
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

  .title {
    margin: 0;
    padding: 8px 14px;
    font-size: 13px;
    font-weight: 600;
    border-bottom: 1px solid var(--separator);
  }

  .note {
    margin: 0;
    padding: 10px 14px;
    color: var(--secondary);
  }
</style>
