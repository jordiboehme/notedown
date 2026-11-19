<script lang="ts" generics="T">
  import { untrack, type Snippet } from "svelte";
  import { scrollToShow, visibleRange, type Range } from "./view";

  // A list that puts only the visible rows (and a few around them) in the DOM; spacers keep the
  // full height. Every row is `rowHeight` pixels high.
  let {
    items,
    rowHeight,
    key,
    row,
    label,
    onRange,
  }: {
    items: T[];
    rowHeight: number;
    key: (item: T) => string | number;
    row: Snippet<[T]>;
    label: string;
    onRange?: (range: Range) => void;
  } = $props();

  let scrollTop = $state(0);
  let height = $state(0);

  const range = $derived(visibleRange({ scrollTop, height, rowHeight, count: items.length }));
  const shown = $derived(items.slice(range.start, range.end));

  let scroller: HTMLDivElement | undefined = $state();

  /** Scrolls row `index` into view (a sort moved the picked row). */
  export function show(index: number) {
    if (!scroller) return;
    scroller.scrollTop = scrollToShow({ index, rowHeight, scrollTop: scroller.scrollTop, height: scroller.clientHeight });
  }

  // Runs when the shown rows or their count change; what the callback reads does not re-run it.
  $effect(() => {
    const shownRange = range;
    untrack(() => onRange?.(shownRange));
  });
</script>

<div
  bind:this={scroller}
  class="scroller"
  role="list"
  aria-label={label}
  bind:clientHeight={height}
  onscroll={(event) => (scrollTop = event.currentTarget.scrollTop)}
>
  <div style:height="{range.top}px" aria-hidden="true"></div>
  {#each shown as item (key(item))}
    <div role="listitem" style:height="{rowHeight}px">{@render row(item)}</div>
  {/each}
  <div style:height="{range.bottom}px" aria-hidden="true"></div>
</div>

<style>
  .scroller {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
  }
</style>
