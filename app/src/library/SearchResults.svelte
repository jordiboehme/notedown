<script lang="ts">
  import { MORE_NAMES, NO_NAMES, NO_TEXT, SCOPE_LABELS, wantsContent, type ResultRow, type Scope } from "./search";
  import type { LibrarySearch } from "./search.svelte";
  import { folderLabel } from "./view";

  // The results of the search in the main area: the scope switch, the name hits and the text
  // hits. A click on a row hands it to the window, which opens or selects what it names.
  let { search, onRow }: { search: LibrarySearch; onRow: (row: ResultRow) => void } = $props();

  const scopes: Scope[] = ["here", "everywhere"];
  const names = $derived(search.rows.filter((row) => row.section === "names"));
  const texts = $derived(search.rows.filter((row) => row.section === "text"));
  const searchesText = $derived(wantsContent(search.text));
</script>

<div class="results">
  <div class="head">
    <div class="switch" role="group" aria-label="Search in">
      {#each scopes as scope (scope)}
        <button
          type="button"
          class:pressed={search.scope === scope}
          aria-pressed={search.scope === scope}
          onclick={() => search.setScope(scope)}
        >
          {SCOPE_LABELS[scope]}
        </button>
      {/each}
    </div>
    {#if search.busy}
      <span class="muted">Searching...</span>
    {/if}
  </div>
  {#if search.error}
    <p class="error" role="alert">{search.error}</p>
  {/if}

  <div class="list">
    <h2 class="section">Names</h2>
    {#each names as row (`${row.kind}:${row.path}`)}
      <button type="button" class="row" onclick={() => onRow(row)}>
        <span class="text">{row.name}</span>
        <span class="text muted">{folderLabel(row.path)}</span>
      </button>
    {:else}
      {#if !search.busy}
        <p class="note">{NO_NAMES}</p>
      {/if}
    {/each}
    {#if search.more}
      <p class="note">{MORE_NAMES}</p>
    {/if}

    <h2 class="section">In the Text</h2>
    {#each texts as row (row.path)}
      <button type="button" class="row hit" onclick={() => onRow(row)}>
        <span class="line">
          <span class="text">{row.name}</span>
          <span class="text muted">{folderLabel(row.path)}</span>
          <span class="muted page">Page {row.page}</span>
        </span>
        <span class="context text muted">{row.context}</span>
      </button>
    {:else}
      {#if !searchesText}
        <p class="note">Type 2 characters or more to search the text.</p>
      {:else if !search.busy}
        <p class="note">{NO_TEXT}</p>
      {/if}
    {/each}
  </div>
</div>

<style>
  .results {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-height: 0;
  }

  .head {
    flex: none;
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 6px 14px;
    border-bottom: 1px solid var(--separator);
  }

  .switch {
    display: inline-flex;
    border: 0.5px solid var(--button-rim);
    border-radius: 5px;
    overflow: hidden;
  }

  .switch button {
    font: inherit;
    font-size: 12px;
    color: var(--primary);
    padding: 1px 12px;
    border: 0;
    background: var(--button);
  }

  .switch button + button {
    border-left: 0.5px solid var(--button-rim);
  }

  .switch button.pressed {
    color: var(--on-brand);
    background: var(--brand);
  }

  .switch button:focus-visible {
    outline: 3px solid color-mix(in srgb, var(--accent), transparent 50%);
    outline-offset: -2px;
  }

  .error {
    flex: none;
    margin: 0;
    padding: 4px 14px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--bad);
    border-bottom: 1px solid var(--separator);
  }

  .list {
    flex: 1;
    overflow-y: auto;
  }

  .section {
    margin: 0;
    padding: 8px 14px 4px;
    font-size: 11px;
    font-weight: 600;
    color: var(--secondary);
    border-bottom: 1px solid var(--separator);
  }

  .row {
    display: grid;
    grid-template-columns: minmax(0, 2fr) minmax(0, 1fr);
    gap: 8px;
    align-items: center;
    width: 100%;
    box-sizing: border-box;
    margin: 0;
    padding: 4px 14px;
    font: inherit;
    color: inherit;
    text-align: left;
    background: transparent;
    border: none;
    border-bottom: 1px solid var(--separator);
  }

  .row.hit {
    display: flex;
    flex-direction: column;
    align-items: stretch;
    gap: 2px;
  }

  .line {
    display: grid;
    grid-template-columns: minmax(0, 2fr) minmax(0, 1fr) 70px;
    gap: 8px;
  }

  .page {
    text-align: right;
    font-variant-numeric: tabular-nums;
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

  .note {
    margin: 0;
    padding: 8px 14px;
    color: var(--secondary);
  }
</style>
