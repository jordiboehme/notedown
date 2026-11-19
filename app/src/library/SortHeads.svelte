<script lang="ts" generics="By extends string">
  // The column heads of a list: a click sorts by the column (ascending, a second click
  // descending); the arrow shows the column and the order. The list's grid places them.
  let {
    columns,
    sort,
    onSort,
  }: {
    columns: readonly { by: By; title: string; end?: boolean }[];
    sort: { by: By; ascending: boolean };
    onSort: (by: By) => void;
  } = $props();

  function arrow(by: By): string {
    if (sort.by !== by) return "";
    return sort.ascending ? "▴" : "▾";
  }

  function label(by: By, title: string): string {
    if (sort.by !== by) return `Sort by ${title}`;
    return `${title}, sorted ${sort.ascending ? "ascending" : "descending"}`;
  }
</script>

{#each columns as column (column.by)}
  <button
    type="button"
    class="head {column.by}"
    class:end={column.end}
    aria-label={label(column.by, column.title)}
    onclick={() => onSort(column.by)}>{column.title}<span class="arrow" aria-hidden="true">{arrow(column.by)}</span></button
  >
{/each}

<style>
  .head {
    min-width: 0;
    overflow: hidden;
    white-space: nowrap;
    font: inherit;
    font-size: 11px;
    text-align: left;
    color: var(--secondary);
    padding: 4px 0;
    border: 0;
    background: none;
  }

  .head:hover {
    color: var(--primary);
  }

  .head:focus-visible {
    outline: 3px solid color-mix(in srgb, var(--accent), transparent 50%);
    outline-offset: -2px;
    border-radius: 3px;
  }

  .head.end {
    text-align: right;
  }

  .arrow {
    margin-left: 4px;
  }
</style>
