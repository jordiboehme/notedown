<script lang="ts">
  import { untrack } from "svelte";
  import type { RecycleItem } from "../lib/library-types";
  import type { Restore } from "../lib/library.svelte";
  import VirtualRows from "./VirtualRows.svelte";
  import { binView, type Bin, type RecycleRow } from "./recycle";
  import { needsNextPage, type Range } from "./view";

  // The recycle bin: Empty in its toolbar (asked in the window first), and a row per item with
  // Restore. A restore into a taken place asks for another name in its row. The first page shows
  // at once; the next page is asked for when the rows near its end come into view.
  let {
    bin,
    rows,
    restores,
    question,
    emptying,
    onMore,
    onRestore,
    onAnswer,
    onCancel,
    onAskEmpty,
    onDropEmpty,
    onEmpty,
  }: {
    bin: Bin;
    rows: RecycleRow[];
    restores: Record<string, Restore>;
    /** The question before Empty while it is asked; null when not. */
    question: string | null;
    /** An Empty runs. */
    emptying: boolean;
    onMore: () => void;
    onRestore: (item: RecycleItem) => void;
    onAnswer: (key: string, typed: string | null) => void;
    onCancel: (key: string) => void;
    onAskEmpty: () => void;
    onDropEmpty: () => void;
    onEmpty: () => void;
  } = $props();

  const ROW = 28;
  const view = $derived(binView(bin));

  function seen(range: Range) {
    if (needsNextPage({ end: range.end, count: rows.length, next: bin.next, busy: bin.busy })) onMore();
  }

  // A page without rows shows no rows to come near: the next page is asked for at once.
  $effect(() => {
    if (bin.loaded && rows.length === 0 && needsNextPage({ end: 0, count: 0, next: bin.next, busy: bin.busy })) untrack(onMore);
  });

  /** The new name is typed in place, the name before the extension selected. */
  function start(field: HTMLInputElement) {
    field.focus();
    const dot = field.value.lastIndexOf(".");
    field.setSelectionRange(0, dot > 0 ? dot : field.value.length);
  }

  function key(event: KeyboardEvent & { currentTarget: HTMLInputElement }, at: string) {
    if (event.key === "Enter") onAnswer(at, event.currentTarget.value);
    else if (event.key === "Escape") onAnswer(at, null);
    else return;
    event.preventDefault();
    event.stopPropagation();
  }

  function answer(event: MouseEvent & { currentTarget: HTMLButtonElement }, at: string) {
    const field = event.currentTarget.closest(".row")?.querySelector("input");
    if (field) onAnswer(at, field.value);
  }
</script>

<h1 class="title">Recycle bin</h1>

<div class="actions" role="toolbar" aria-label="Recycle bin actions">
  {#if question !== null}
    <span class="question" role="alert">{question}</span>
    <button type="button" class="button danger" onclick={onEmpty}>Empty</button>
    <button type="button" class="button" onclick={onDropEmpty}>Cancel</button>
  {:else}
    <button type="button" class="button" disabled={!view.canEmpty || emptying} onclick={onAskEmpty}>Empty Recycle Bin</button>
  {/if}
</div>

<div class="heads" aria-hidden="true">
  <span>Name</span>
  <span>Was in</span>
  <span>Deleted</span>
  <span class="size">Size</span>
  <span></span>
</div>

<div class="body">
  {#if bin.error}
    <p class="note problem" role="status">{bin.error}</p>
  {/if}

  {#if view.loading || (rows.length === 0 && bin.next !== null && !bin.error)}
    <p class="note">Loading...</p>
  {:else if view.empty}
    <p class="note">The recycle bin is empty.</p>
  {:else if rows.length > 0}
    <VirtualRows items={rows} rowHeight={ROW} key={(row: RecycleRow) => row.key} label="Deleted files and folders" onRange={seen}>
      {#snippet row(row: RecycleRow)}
        {@const restore = restores[row.key]}
        <div class="row" class:asking={restore?.state === "asking"}>
          <span class="name">
            <svg class="icon" viewBox="0 0 16 16" aria-hidden="true">
              {#if row.kind === "folder"}
                <path class="folder" d="M1.5 4.5v8.5h13V5.5H7.5L6 3.5H2z" />
              {:else}
                <path class="file" d="M3.5 1.5h6l3 3v10h-9z M9.5 1.5v3h3" />
              {/if}
            </svg>
            {#if restore?.state === "asking"}
              <input
                class="field"
                type="text"
                value={restore.name}
                aria-label="Restore as"
                spellcheck="false"
                autocomplete="off"
                {@attach start}
                onkeydown={(event) => key(event, row.key)}
              />
            {:else}
              <span class="text">{row.name}</span>
            {/if}
          </span>
          {#if restore?.state === "asking"}
            <span class="clash" role="status">{restore.text ?? ""}</span>
            <span class="buttons">
              <button type="button" class="button" onclick={(event) => answer(event, row.key)}>Restore</button>
              <button type="button" class="button" onclick={() => onCancel(row.key)}>Cancel</button>
            </span>
          {:else}
            <span class="dim text">{row.folder}</span>
            <span class="dim text">{row.deleted}</span>
            <span class="dim size">{row.size}</span>
            <span class="buttons">
              <button type="button" class="button" disabled={restore !== undefined} onclick={() => onRestore(row.item)}>
                {restore ? "Restoring..." : "Restore"}
              </button>
            </span>
          {/if}
        </div>
      {/snippet}
    </VirtualRows>
    {#if bin.busy && bin.next}
      <p class="note more">Loading more...</p>
    {/if}
  {/if}
</div>

<style>
  .title {
    margin: 0;
    padding: 8px 14px;
    font-size: 13px;
    font-weight: 600;
    border-bottom: 1px solid var(--separator);
  }

  .actions {
    flex: none;
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 5px 14px;
    border-bottom: 1px solid var(--separator);
  }

  .question {
    margin-right: 4px;
  }

  .button:disabled {
    color: var(--tertiary);
  }

  .danger {
    color: var(--bad);
  }

  .body {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-height: 0;
  }

  .heads,
  .row {
    display: grid;
    grid-template-columns: minmax(0, 1.4fr) minmax(0, 1fr) 110px 80px 130px;
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
    height: 100%;
    box-sizing: border-box;
    border-bottom: 1px solid var(--separator);
  }

  /* Paper and ink, as the selection of the other lists. */
  .row.asking {
    background: color-mix(in srgb, var(--brand), transparent 86%);
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

  .dim {
    color: var(--secondary);
  }

  .size {
    text-align: right;
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }

  .clash {
    grid-column: span 3;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--secondary);
  }

  .buttons {
    display: flex;
    justify-content: flex-end;
    gap: 4px;
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

  .field {
    flex: 1;
    min-width: 0;
    font: inherit;
    color: var(--primary);
    padding: 0 3px;
    border: 1px solid var(--accent);
    border-radius: 3px;
    background: var(--box);
    user-select: text;
    outline: none;
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
