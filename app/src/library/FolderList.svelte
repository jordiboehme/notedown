<script lang="ts">
  import { tick, untrack } from "svelte";
  import type { Entry, FolderState } from "../lib/library-types";
  import NameField from "./NameField.svelte";
  import SortHeads from "./SortHeads.svelte";
  import VirtualRows from "./VirtualRows.svelte";
  import { opensReview } from "./actions";
  import { noteMenu } from "./review";
  import { drag, pressOn } from "./drag.svelte";
  import { loadsRest, nextSort } from "./sort";
  import { dateText, listRows, needsNextPage, sizeText, type Range, type Sort } from "./view";

  // The selected folder: the column heads and the rows. The first page shows at
  // once; the next page is asked for when the rows near its end come into view. A click picks
  // a row (for the file actions), a double click opens a folder or the review of a .note file
  // (Enter too, for a .note), a press and a drag moves it.
  let {
    path,
    folder,
    next,
    busy,
    sort,
    picked,
    renaming,
    onSort,
    onOpen,
    onOpenNote,
    onMore,
    onPick,
    onRename,
  }: {
    path: string;
    folder: FolderState | undefined;
    next: string | null;
    busy: boolean;
    sort: Sort;
    picked: string | null;
    renaming: string | null;
    onSort: (sort: Sort) => void;
    onOpen: (path: string) => void;
    onOpenNote: (path: string) => void;
    onMore: () => void;
    onPick: (path: string) => void;
    onRename: (entry: Entry, typed: string | null) => void;
  } = $props();

  const ROW = 24;
  const view = $derived(folder ? listRows(folder, sort) : { rows: [], sortable: false });

  /** A head was clicked in this folder: it loads all its pages, so the sort covers every row. */
  let clicked = $state(false);

  let list: ReturnType<typeof VirtualRows<Entry>> | undefined = $state();

  function sortBy(by: Sort["by"]) {
    clicked = true;
    onSort(nextSort(sort, by));
    // The picked row stays picked; the sort may move it out of view.
    void tick().then(() => {
      if (picked !== null) list?.show(view.rows.findIndex((entry) => entry.path === picked));
    });
  }

  function seen(range: Range) {
    if (needsNextPage({ end: range.end, count: view.rows.length, next, busy })) onMore();
  }

  // A page without rows shows no rows to come near: the next page is asked for at once. A sort
  // that is not the order of the pages asks for every page; the rows are sorted once all came.
  $effect(() => {
    if (!folder || folder.error !== null) return;
    const all = view.rows.length === 0 || loadsRest("folder", sort, clicked);
    if (all && needsNextPage({ end: view.rows.length, count: view.rows.length, next, busy })) untrack(onMore);
  });
</script>

<div class="heads" role="group" aria-label="Sort">
  <SortHeads
    columns={[
      { by: "name", title: "Name" },
      { by: "size", title: "Size", end: true },
      { by: "date", title: "Changed" },
    ]}
    {sort}
    onSort={sortBy}
  />
</div>

<!-- The rows of the shown folder: a drop anywhere here that is not on a folder row goes into it. -->
<div class="body" data-folder={path}>
  {#if folder?.error}
    <p class="note problem" role="status">{folder.error}</p>
  {/if}

  {#if !folder || (folder.entries.length === 0 && !folder.complete && !folder.error)}
    <p class="note">Loading...</p>
  {:else if folder.entries.length === 0 && folder.complete}
    <p class="note">This folder is empty.</p>
  {:else}
    <VirtualRows bind:this={list} items={view.rows} rowHeight={ROW} key={(entry: Entry) => entry.id} label="Files and folders" onRange={seen}>
      {#snippet row(entry: Entry)}
        {#snippet name()}
          <svg class="icon" viewBox="0 0 16 16" aria-hidden="true">
            {#if entry.kind === "folder"}
              <path class="folder" d="M1.5 4.5v8.5h13V5.5H7.5L6 3.5H2z" />
            {:else}
              <path class="file" d="M3.5 1.5h6l3 3v10h-9z M9.5 1.5v3h3" />
            {/if}
          </svg>
        {/snippet}
        {#if renaming === entry.path}
          <div class="row picked" data-folder={entry.kind === "folder" ? entry.path : undefined}>
            <span class="name">
              {@render name()}
              <NameField name={entry.name} label="New name" onDone={(typed) => onRename(entry, typed)} />
            </span>
            <span class="size">{sizeText(entry)}</span>
            <span class="date">{dateText(entry.mtimeMs)}</span>
          </div>
        {:else}
          <button
            type="button"
            class="row"
            class:picked={picked === entry.path}
            class:target={drag.current?.over === entry.path}
            data-folder={entry.kind === "folder" ? entry.path : undefined}
            onclick={() => onPick(entry.path)}
            ondblclick={() => (entry.kind === "folder" ? onOpen(entry.path) : opensReview(entry) && onOpenNote(entry.path))}
            onkeydown={(event) => {
              if (event.key !== "Enter" || !opensReview(entry)) return;
              event.preventDefault();
              onOpenNote(entry.path);
            }}
            oncontextmenu={(event) => opensReview(entry) && noteMenu(event, entry.path)}
            onpointerdown={(event) => pressOn(entry, event)}
          >
            <span class="name">
              {@render name()}
              <span class="text">{entry.name}</span>
            </span>
            <span class="size">{sizeText(entry)}</span>
            <span class="date">{dateText(entry.mtimeMs)}</span>
          </button>
        {/if}
      {/snippet}
    </VirtualRows>
    {#if busy && next}
      <p class="note more">Loading more...</p>
    {/if}
  {/if}
</div>

<style>
  .body {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-height: 0;
  }

  .heads,
  .row {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 90px 170px;
    align-items: center;
    gap: 8px;
    padding: 0 14px;
  }

  .heads {
    flex: none;
    border-bottom: 1px solid var(--separator);
  }

  .row {
    width: 100%;
    height: 100%;
    box-sizing: border-box;
    font: inherit;
    color: var(--primary);
    text-align: left;
    border: 0;
    border-bottom: 1px solid var(--separator);
    background: none;
  }

  /* Paper and ink, as the sidebar's selection. */
  .row.picked {
    background: color-mix(in srgb, var(--brand), transparent 86%);
  }

  /* The folder a dragged row goes into when it is let go. */
  .row.target {
    outline: 2px solid var(--brand);
    outline-offset: -2px;
  }

  .row:focus-visible {
    outline: 3px solid color-mix(in srgb, var(--accent), transparent 50%);
    outline-offset: -3px;
  }

  .name {
    display: flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
  }

  .text {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .icon {
    flex: none;
    width: 16px;
    height: 16px;
    fill: none;
    stroke: var(--secondary);
    stroke-width: 1;
  }

  .icon .folder {
    fill: color-mix(in srgb, var(--brand), transparent 85%);
    stroke: var(--brand);
  }

  .size,
  .date {
    color: var(--secondary);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .size {
    text-align: right;
    font-variant-numeric: tabular-nums;
  }

  .note {
    margin: 0;
    padding: 10px 14px;
    color: var(--secondary);
  }

  .note.problem {
    color: var(--bad);
  }

  .note.more {
    flex: none;
    padding: 4px 14px;
    font-size: 11px;
  }
</style>
