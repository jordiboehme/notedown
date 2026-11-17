<script lang="ts">
  import { untrack } from "svelte";
  import type { Entry, FolderState } from "../lib/library-types";
  import VirtualRows from "./VirtualRows.svelte";
  import { dateText, listRows, needsNextPage, pathBar, sizeText, type Range, type Sort } from "./view";

  // The selected folder: the path bar, the column heads and the rows. The first page shows at
  // once; the next page is asked for when the rows near its end come into view.
  let {
    path,
    folder,
    next,
    busy,
    sort,
    onSort,
    onOpen,
    onMore,
  }: {
    path: string;
    folder: FolderState | undefined;
    next: string | null;
    busy: boolean;
    sort: Sort;
    onSort: (sort: Sort) => void;
    onOpen: (path: string) => void;
    onMore: () => void;
  } = $props();

  const ROW = 24;
  const crumbs = $derived(pathBar(path));
  const view = $derived(folder ? listRows(folder, sort) : { rows: [], sortable: false });

  function sortBy(by: Sort["by"]) {
    if (!view.sortable) return;
    onSort(sort.by === by ? { by, ascending: !sort.ascending } : { by, ascending: by === "name" });
  }

  function seen(range: Range) {
    if (needsNextPage({ end: range.end, count: view.rows.length, next, busy })) onMore();
  }

  // A page without rows shows no rows to come near: the next page is asked for at once.
  $effect(() => {
    if (folder && view.rows.length === 0 && needsNextPage({ end: 0, count: 0, next, busy })) untrack(onMore);
  });

  function arrow(by: Sort["by"]): string {
    if (!view.sortable || sort.by !== by) return "";
    return sort.ascending ? "▴" : "▾";
  }
</script>

<nav class="path" aria-label="Path">
  {#each crumbs as crumb, i (crumb.path)}
    {#if i > 0}<span class="sep" aria-hidden="true">/</span>{/if}
    {#if i === crumbs.length - 1}
      <span class="crumb current" aria-current="page">{crumb.label}</span>
    {:else}
      <button type="button" class="crumb" onclick={() => onOpen(crumb.path)}>{crumb.label}</button>
    {/if}
  {/each}
</nav>

<div class="heads" role="group" aria-label="Sort">
  {#each [["name", "Name"], ["size", "Size"], ["date", "Changed"]] as const as [by, title] (by)}
    <button
      type="button"
      class="head {by}"
      disabled={!view.sortable}
      title={view.sortable ? undefined : "The folder can be sorted once it is loaded."}
      onclick={() => sortBy(by)}
    >
      {title}<span class="arrow" aria-hidden="true">{arrow(by)}</span>
    </button>
  {/each}
</div>

{#if folder?.error}
  <p class="note problem" role="status">{folder.error}</p>
{/if}

{#if !folder || (folder.entries.length === 0 && !folder.complete && !folder.error)}
  <p class="note">Loading...</p>
{:else if folder.entries.length === 0 && folder.complete}
  <p class="note">This folder is empty.</p>
{:else}
  <VirtualRows items={view.rows} rowHeight={ROW} key={(entry: Entry) => entry.id} label="Files and folders" onRange={seen}>
    {#snippet row(entry: Entry)}
      <div
        class="row"
        data-folder={entry.kind === "folder" ? entry.path : undefined}
        ondblclick={() => entry.kind === "folder" && onOpen(entry.path)}
        role="presentation"
      >
        <span class="name">
          <svg class="icon" viewBox="0 0 16 16" aria-hidden="true">
            {#if entry.kind === "folder"}
              <path class="folder" d="M1.5 4.5v8.5h13V5.5H7.5L6 3.5H2z" />
            {:else}
              <path class="file" d="M3.5 1.5h6l3 3v10h-9z M9.5 1.5v3h3" />
            {/if}
          </svg>
          <span class="text">{entry.name}</span>
        </span>
        <span class="size">{sizeText(entry)}</span>
        <span class="date">{dateText(entry.mtimeMs)}</span>
      </div>
    {/snippet}
  </VirtualRows>
  {#if busy && next}
    <p class="note more">Loading more...</p>
  {/if}
{/if}

<style>
  .path {
    flex: none;
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 4px;
    padding: 8px 14px;
    border-bottom: 1px solid var(--separator);
  }

  .crumb {
    font: inherit;
    color: var(--secondary);
    padding: 0 2px;
    border: 0;
    background: none;
    border-radius: 4px;
  }

  button.crumb:hover {
    color: var(--primary);
  }

  .crumb.current {
    color: var(--primary);
    font-weight: 600;
  }

  .sep {
    color: var(--tertiary);
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

  .head {
    font: inherit;
    font-size: 11px;
    text-align: left;
    color: var(--secondary);
    padding: 4px 0;
    border: 0;
    background: none;
  }

  .head:disabled {
    color: var(--tertiary);
  }

  .head.size {
    text-align: right;
  }

  .arrow {
    margin-left: 4px;
  }

  .row {
    height: 100%;
    box-sizing: border-box;
    border-bottom: 1px solid var(--separator);
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
