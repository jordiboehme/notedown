<script lang="ts">
  import { pathBar } from "./view";

  // The path bar: each folder above the shown one is a button that shows it.
  let { path, onOpen }: { path: string; onOpen: (path: string) => void } = $props();

  const crumbs = $derived(pathBar(path));
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
</style>
