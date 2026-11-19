<script lang="ts">
  import type { NoteSummary } from "../lib/library-types";
  import SortHeads from "./SortHeads.svelte";
  import VirtualRows from "./VirtualRows.svelte";
  import { noteMenu } from "./review";
  import { nextSort, sortNotes, type NotesSort } from "./sort";
  import { dateText, folderLabel } from "./view";

  // New notes or all notes as rows, sorted by a column; a click opens the review of the note
  // and a right click its context menu (Rust's). Titles are text.
  let {
    title,
    notes,
    empty,
    sort,
    onSort,
    onOpen,
    onMarkAllSeen,
  }: {
    title: string;
    notes: NoteSummary[] | null;
    empty: string;
    sort: NotesSort;
    onSort: (sort: NotesSort) => void;
    onOpen: (note: NoteSummary) => void;
    /** Given only for the New list: it shows the Mark All as Seen button. */
    onMarkAllSeen?: () => void;
  } = $props();

  const rows = $derived(notes ? sortNotes(notes, sort) : []);
</script>

<div class="head">
  <h1 class="title">{title}</h1>
  {#if onMarkAllSeen}
    <button type="button" class="mark" disabled={!notes?.length} onclick={onMarkAllSeen}>Mark All as Seen</button>
  {/if}
</div>

<div class="heads" role="group" aria-label="Sort">
  <SortHeads
    columns={[
      { by: "name", title: "Name" },
      { by: "folder", title: "Folder" },
      { by: "pages", title: "Pages", end: true },
      { by: "date", title: "Changed" },
    ]}
    {sort}
    onSort={(by) => onSort(nextSort(sort, by))}
  />
</div>

{#if notes === null}
  <p class="note">Loading...</p>
{:else if notes.length === 0}
  <p class="note">{empty}</p>
{:else}
  <VirtualRows items={rows} rowHeight={24} key={(note: NoteSummary) => note.key} label={title}>
    {#snippet row(note: NoteSummary)}
      <button type="button" class="row" onclick={() => onOpen(note)} oncontextmenu={(event) => noteMenu(event, note.path)}>
        <span class="text">{note.title}</span>
        <span class="text muted">{folderLabel(note.path)}</span>
        <span class="pages muted">{note.pages}</span>
        <span class="text muted">{dateText(note.changedMs)}</span>
      </button>
    {/snippet}
  </VirtualRows>
{/if}

<style>
  .head {
    flex: none;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    padding: 0 14px 0 0;
    border-bottom: 1px solid var(--separator);
  }

  .title {
    margin: 0;
    padding: 8px 14px;
    font-size: 13px;
    font-weight: 600;
  }

  .mark {
    font: inherit;
    font-size: 12px;
    padding: 2px 10px;
  }

  .heads,
  .row {
    display: grid;
    grid-template-columns: minmax(0, 2fr) minmax(0, 1fr) 50px 170px;
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
    margin: 0;
    font: inherit;
    color: inherit;
    text-align: left;
    background: transparent;
    border: none;
    border-bottom: 1px solid var(--separator);
  }

  .row:hover {
    background: var(--track);
  }

  .text {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .muted {
    color: var(--secondary);
  }

  .pages {
    text-align: right;
    font-variant-numeric: tabular-nums;
  }

  .note {
    margin: 0;
    padding: 10px 14px;
    color: var(--secondary);
  }
</style>
