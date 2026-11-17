<script lang="ts">
  import { untrack } from "svelte";
  import type { Entry } from "../lib/library-types";
  import NameField from "./NameField.svelte";
  import VirtualRows from "./VirtualRows.svelte";
  import { drag, pressOn } from "./drag.svelte";
  import { needsNextPage, type Column, type ColumnRow, type Range } from "./view";

  // One column of the columns view: a lazy, virtual list of one folder. The next page is asked
  // for when the rows near its end come into view. A row that is not a device tree can be
  // dragged onto a folder; the row being renamed shows a field.
  let {
    column,
    next,
    busy,
    renaming,
    onSelect,
    onMore,
    onRename,
  }: {
    column: Column;
    next: string | null;
    busy: boolean;
    renaming: string | null;
    onSelect: (path: string, kind: "file" | "folder") => void;
    onMore: () => void;
    onRename: (entry: Entry, typed: string | null) => void;
  } = $props();

  const ROW = 24;
  const label = $derived(column.path === null ? "Folders" : "Files and folders");

  function seen(range: Range) {
    if (needsNextPage({ end: range.end, count: column.rows.length, next, busy })) onMore();
  }

  // A page without rows has none to come near: the next page is asked for at once.
  $effect(() => {
    if (column.rows.length === 0 && needsNextPage({ end: 0, count: 0, next, busy })) untrack(onMore);
  });
</script>

<div
  class="column"
  role="group"
  aria-label={column.path === null ? "Device folders" : "Folder"}
  class:target={column.path !== null && drag.current?.over === column.path}
  data-folder={column.path ?? undefined}
>
  {#if column.error}
    <p class="note problem" role="status">{column.error}</p>
  {/if}
  {#if column.rows.length > 0}
    <VirtualRows items={column.rows} rowHeight={ROW} key={(row: ColumnRow) => row.path} {label} onRange={seen}>
      {#snippet row(entry: ColumnRow)}
        {#snippet icon()}
          <svg class="icon" class:folder={entry.kind === "folder"} viewBox="0 0 16 16" aria-hidden="true">
            {#if entry.kind === "folder"}
              <path d="M1.5 4.5v8.5h13V5.5H7.5L6 3.5H2z" />
            {:else}
              <path d="M3.5 1.5h6l3 3v10h-9z M9.5 1.5v3h3" />
            {/if}
          </svg>
        {/snippet}
        {#if entry.entry && renaming === entry.path}
          {@const renamed = entry.entry}
          <div class="row selected" data-folder={entry.kind === "folder" ? entry.path : undefined}>
            {@render icon()}
            <NameField name={entry.name} label="New name" onDone={(typed) => onRename(renamed, typed)} />
          </div>
        {:else}
          <button
            type="button"
            class="row"
            class:selected={entry.selected}
            class:file={entry.kind === "file"}
            class:target={drag.current?.over === entry.path}
            data-folder={entry.kind === "folder" ? entry.path : undefined}
            aria-current={entry.selected ? "true" : undefined}
            onclick={() => onSelect(entry.path, entry.kind)}
            onpointerdown={(event) => entry.entry && pressOn(entry.entry, event)}
          >
            {@render icon()}
            <span class="text">{entry.name}</span>
            {#if entry.kind === "folder"}
              <svg class="more" viewBox="0 0 10 10" aria-hidden="true"><path d="M3 1.5 7 5 3 8.5z" /></svg>
            {/if}
          </button>
        {/if}
      {/snippet}
    </VirtualRows>
  {:else if column.loading}
    <p class="note">Loading...</p>
  {:else if column.empty}
    <p class="note">This folder is empty.</p>
  {/if}
  {#if busy && next && column.rows.length > 0}
    <p class="note more-note">Loading more...</p>
  {/if}
</div>

<style>
  .column {
    flex: none;
    display: flex;
    flex-direction: column;
    width: 230px;
    min-height: 0;
    box-sizing: border-box;
    border-right: 1px solid var(--separator);
  }

  .row {
    display: flex;
    align-items: center;
    gap: 6px;
    width: 100%;
    height: 100%;
    box-sizing: border-box;
    padding: 0 8px 0 10px;
    font: inherit;
    color: var(--primary);
    text-align: left;
    border: 0;
    background: none;
  }

  .row.file {
    color: var(--secondary);
  }

  /* Paper and ink, as the sidebar's selection. */
  .row.selected {
    background: color-mix(in srgb, var(--brand), transparent 86%);
    font-weight: 600;
  }

  /* The folder a dragged row goes into when it is let go: its row or its column. */
  .column.target,
  .row.target {
    outline: 2px solid var(--brand);
    outline-offset: -2px;
  }

  .text {
    flex: 1;
    min-width: 0;
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

  .icon.folder {
    fill: color-mix(in srgb, var(--brand), transparent 85%);
    stroke: var(--brand);
  }

  .more {
    flex: none;
    width: 9px;
    height: 9px;
    fill: var(--tertiary);
  }

  .note {
    margin: 0;
    padding: 8px 12px;
    color: var(--secondary);
  }

  .note.problem {
    color: var(--bad);
  }

  .more-note {
    flex: none;
    padding: 4px 12px;
    font-size: 11px;
  }

  button:focus-visible {
    outline: 3px solid color-mix(in srgb, var(--accent), transparent 50%);
    outline-offset: -2px;
  }
</style>
