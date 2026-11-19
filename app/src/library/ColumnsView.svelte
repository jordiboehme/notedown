<script lang="ts">
  import { tick } from "svelte";
  import { isBusy, library, listNext } from "../lib/library.svelte";
  import ColumnList from "./ColumnList.svelte";
  import type { Entry } from "../lib/library-types";
  import { listingKey, type Column } from "./view";

  // The columns view: one column per level of the selected folder, the device
  // trees first. The columns hold a folder each; the window lists them when they are shown.
  let {
    columns,
    renaming,
    onSelect,
    onFile,
    onOpenNote,
    onRename,
  }: {
    columns: Column[];
    renaming: string | null;
    onSelect: (path: string) => void;
    onFile: (path: string) => void;
    onOpenNote: (path: string) => void;
    onRename: (entry: Entry, typed: string | null) => void;
  } = $props();

  let strip: HTMLDivElement | undefined = $state();

  // A new level opens at the right: keep the last column in view.
  $effect(() => {
    void columns.length;
    void tick().then(() => strip?.scrollTo({ left: strip.scrollWidth }));
  });
</script>

<div class="columns" bind:this={strip}>
  {#each columns as column (column.path ?? "")}
    <ColumnList
      {column}
      next={column.path === null ? null : (library.next[listingKey(column.path, false)] ?? null)}
      busy={column.path === null ? false : isBusy(column.path, false)}
      {renaming}
      {onRename}
      {onOpenNote}
      onSelect={(picked, kind) => (kind === "folder" ? onSelect(picked) : onFile(picked))}
      onMore={() => column.path !== null && void listNext(column.path, false)}
    />
  {/each}
</div>

<style>
  .columns {
    flex: 1;
    display: flex;
    min-height: 0;
    overflow-x: auto;
    overflow-y: hidden;
  }
</style>
