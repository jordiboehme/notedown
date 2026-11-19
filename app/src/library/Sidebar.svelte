<script lang="ts">
  import type { LibraryStateView } from "../lib/library-types";
  import StateNotice from "./StateNotice.svelte";
  import { drag } from "./drag.svelte";
  import type { SidebarItem, TreeRow } from "./view";

  // The sidebar: New and All notes, the device trees (folders only, a folder listed when it
  // opens) and the recycle bin. While the library cannot be read, its state stands in for them.
  let {
    notes,
    tree,
    rows,
    bin,
    view,
    onPick,
    onSelect,
    onToggle,
    onSettings,
    onNewMenu,
  }: {
    notes: SidebarItem[];
    tree: boolean;
    rows: TreeRow[];
    bin: SidebarItem | null;
    view: LibraryStateView | null;
    onPick: (id: SidebarItem["id"]) => void;
    onSelect: (path: string) => void;
    onToggle: (path: string) => void;
    onSettings: (page: string | null) => void;
    onNewMenu: () => void;
  } = $props();
</script>

{#snippet item(entry: SidebarItem)}
  <button
    type="button"
    class="item"
    class:selected={entry.selected}
    aria-current={entry.selected ? "page" : undefined}
    onclick={() => onPick(entry.id)}
    oncontextmenu={entry.id === "new"
      ? (event) => {
          event.preventDefault();
          onNewMenu();
        }
      : undefined}
  >
    <span class="text">{entry.label}</span>
    {#if entry.count}<span class="count" class:fresh={entry.id === "new"}>{entry.count}</span>{/if}
  </button>
{/snippet}

<nav class="sidebar" aria-label="Library">
  <h2>Notes</h2>
  {#each notes as entry (entry.id)}{@render item(entry)}{/each}

  <h2>Library</h2>
  {#if tree}
    <ul class="tree" role="tree" aria-label="Folders">
      {#each rows as row (row.path)}
        <li
          class="node"
          class:selected={row.selected}
          class:target={drag.current?.over === row.path}
          role="treeitem"
          aria-selected={row.selected}
          aria-expanded={row.empty ? undefined : row.open}
          data-folder={row.path}
          style:padding-left="{6 + row.depth * 14}px"
          {@attach (node) => {
            // The selected folder comes into view (a note opened elsewhere selects its folder).
            if (row.selected) node.scrollIntoView?.({ block: "nearest" });
          }}
        >
          {#if row.empty}
            <span class="twisty" aria-hidden="true"></span>
          {:else}
            <button
              type="button"
              class="twisty"
              aria-label={row.open ? `Close ${row.name}` : `Open ${row.name}`}
              onclick={() => onToggle(row.path)}
            >
              <svg viewBox="0 0 10 10" aria-hidden="true" class:open={row.open}><path d="M3 1.5 7 5 3 8.5z" /></svg>
            </button>
          {/if}
          <button type="button" class="label" onclick={() => onSelect(row.path)}>
            <span class="text">{row.name}</span>
          </button>
          {#if row.loading}<span class="busy" aria-label="Loading">...</span>{/if}
        </li>
        {#if row.open && row.error}
          <li class="problem" role="none" style:padding-left="{26 + row.depth * 14}px">{row.error}</li>
        {/if}
      {/each}
    </ul>
    {#if bin}{@render item(bin)}{/if}
  {:else}
    <StateNotice {view} {onSettings} compact />
  {/if}
</nav>

<style>
  .sidebar {
    flex: none;
    width: 220px;
    padding: 6px 0 12px;
    overflow-y: auto;
    box-sizing: border-box;
    background: var(--sidebar);
    border-right: 1px solid var(--separator);
  }

  h2 {
    margin: 0;
    padding: 10px 14px 3px;
    font-size: 11px;
    font-weight: 600;
    color: var(--secondary);
  }

  .item,
  .node {
    display: flex;
    align-items: center;
    gap: 2px;
    margin: 0 6px;
    border-radius: 5px;
  }

  .item {
    width: calc(100% - 12px);
    justify-content: space-between;
    font: inherit;
    color: var(--primary);
    text-align: left;
    padding: 3px 8px;
    border: 0;
    background: none;
  }

  /* Paper and ink: the selection in Notedown's own colours. */
  .item.selected,
  .node.selected {
    background: color-mix(in srgb, var(--brand), transparent 86%);
    font-weight: 600;
  }

  /* The folder a dragged row goes into when it is let go. */
  .node.target {
    outline: 2px solid var(--brand);
    outline-offset: -2px;
  }

  .count {
    color: var(--secondary);
    font-variant-numeric: tabular-nums;
  }

  .count.fresh {
    padding: 0 6px;
    border-radius: 8px;
    color: var(--on-brand);
    background: var(--brand);
    font-size: 11px;
  }

  .tree {
    list-style: none;
    margin: 0;
    padding: 0;
  }

  .twisty {
    flex: none;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 16px;
    height: 20px;
    padding: 0;
    border: 0;
    background: none;
  }

  .twisty svg {
    width: 9px;
    height: 9px;
    fill: var(--secondary);
    transition: transform 0.12s;
  }

  .twisty svg.open {
    transform: rotate(90deg);
  }

  .label {
    flex: 1;
    min-width: 0;
    font: inherit;
    color: var(--primary);
    text-align: left;
    padding: 2px 4px;
    border: 0;
    background: none;
  }

  .text {
    display: block;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .busy {
    flex: none;
    padding-right: 6px;
    color: var(--tertiary);
  }

  .problem {
    margin: 0 6px;
    padding: 2px 8px 4px;
    font-size: 11px;
    color: var(--bad);
  }

  button:focus-visible {
    outline: 3px solid color-mix(in srgb, var(--accent), transparent 50%);
    outline-offset: -2px;
    border-radius: 5px;
  }
</style>
