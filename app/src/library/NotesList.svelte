<script lang="ts">
  import type { NoteSummary } from "../lib/library-types";
  import VirtualRows from "./VirtualRows.svelte";
  import { dateText, folderLabel } from "./view";

  // New notes or all notes as rows; a click opens the review of the note. Titles are text.
  let {
    title,
    notes,
    empty,
    onOpen,
  }: { title: string; notes: NoteSummary[] | null; empty: string; onOpen: (note: NoteSummary) => void } = $props();
</script>

<h1 class="title">{title}</h1>

<div class="heads" role="presentation">
  <span>Name</span><span>Folder</span><span class="pages">Pages</span><span>Changed</span>
</div>

{#if notes === null}
  <p class="note">Loading...</p>
{:else if notes.length === 0}
  <p class="note">{empty}</p>
{:else}
  <VirtualRows items={notes} rowHeight={24} key={(note: NoteSummary) => note.key} label={title}>
    {#snippet row(note: NoteSummary)}
      <button type="button" class="row" onclick={() => onOpen(note)}>
        <span class="text">{note.title}</span>
        <span class="text muted">{folderLabel(note.path)}</span>
        <span class="pages muted">{note.pages}</span>
        <span class="text muted">{dateText(note.changedMs)}</span>
      </button>
    {/snippet}
  </VirtualRows>
{/if}

<style>
  .title {
    flex: none;
    margin: 0;
    padding: 8px 14px;
    font-size: 13px;
    font-weight: 600;
    border-bottom: 1px solid var(--separator);
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
    padding-top: 4px;
    padding-bottom: 4px;
    font-size: 11px;
    color: var(--secondary);
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
