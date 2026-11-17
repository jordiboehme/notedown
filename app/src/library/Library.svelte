<script lang="ts">
  import { onMount, untrack } from "svelte";
  import { SvelteSet } from "svelte/reactivity";
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { isBusy, library, listFolder, listNext, showListings, startLibrary } from "../lib/library.svelte";
  import { NOTES_EVENT, type NoteSummary } from "../lib/library-types";
  import { startStatus, status } from "../lib/status.svelte";
  import FolderList from "./FolderList.svelte";
  import NotesList from "./NotesList.svelte";
  import Sidebar from "./Sidebar.svelte";
  import StateNotice from "./StateNotice.svelte";
  import StatusLine from "./StatusLine.svelte";
  import {
    listingKey,
    pathBar,
    shownListings,
    sidebarItems,
    treeView,
    type Selection,
    type SidebarItem,
    type Sort,
  } from "./view";
  import "../lib/system.css";
  import "./library.css";

  // The library window: the status line, the sidebar with the notes and the device trees, and
  // the selected folder or notes. It wires the store to Rust; the view models decide.
  let selection = $state<Selection>({ kind: "new" });
  const open = new SvelteSet<string>();
  let sort = $state<Sort>({ by: "name", ascending: true });
  let newNotes = $state<NoteSummary[] | null>(null);
  let allNotes = $state<NoteSummary[] | null>(null);
  let now = $state(Date.now());

  const ready = $derived(library.view?.state === "ready");
  const selectedPath = $derived(selection.kind === "folder" ? selection.path : null);
  const tree = $derived(treeView(library.listings, open, selectedPath));
  const items = $derived(
    sidebarItems(library.view, { newNotes: newNotes?.length ?? null, allNotes: allNotes?.length ?? null }, selection),
  );

  function report(command: string) {
    return (error: unknown) => console.error(`notedown: ${command} failed`, error);
  }

  // Rust follows only what the window shows: tell it on every change, and again after a new
  // binding (the generation moves).
  $effect(() => {
    void library.generation;
    const shown = shownListings(selection, open);
    untrack(() => showListings(shown));
  });

  // The tree lists a folder when it opens, and the rest of its pages after the first.
  $effect(() => {
    if (!ready) return;
    const { missing, more } = tree;
    untrack(() => {
      for (const path of missing) void listFolder(path, true);
      for (const path of more) void listNext(path, true);
    });
  });

  // The selected folder: its first page at once (again after a new binding).
  $effect(() => {
    if (!ready || selection.kind !== "folder") return;
    const path = selection.path;
    if (library.listings[listingKey(path, false)] === undefined) untrack(() => void listFolder(path, false));
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
    const stops = [startLibrary(), startStatus(), listen(NOTES_EVENT, loadNotes)];
    loadNotes();
    const clock = setInterval(() => (now = Date.now()), 30_000);
    return () => {
      clearInterval(clock);
      for (const stop of stops) void stop.then((unlisten) => unlisten());
    };
  });

  /** Shows a folder and opens the folders above it in the tree. */
  function select(path: string) {
    selection = { kind: "folder", path };
    for (const crumb of pathBar(path).slice(0, -1)) open.add(crumb.path);
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

<svelte:window onfocus={() => (now = Date.now())} />

<div class="library window">
  <StatusLine status={status.current} {now} onSetUp={() => settings("setup")} />
  <div class="body">
    <Sidebar
      notes={items.notes}
      tree={items.tree}
      rows={tree.rows}
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
        {#key `${path}:${library.generation}`}
          <FolderList
            {path}
            folder={library.listings[listingKey(path, false)]}
            next={library.next[listingKey(path, false)] ?? null}
            busy={isBusy(path, false)}
            {sort}
            onSort={(next) => (sort = next)}
            onOpen={select}
            onMore={() => void listNext(path, false)}
          />
        {/key}
      {/if}
    </main>
  </div>
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
