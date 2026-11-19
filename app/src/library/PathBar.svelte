<script lang="ts">
  import type { PathPart } from "./view";

  // The path of what the window shows, in the toolbar: each folder is a button that shows it,
  // the name at the end (an open note, New, All notes, the bin) is text. A long path is cut
  // from the left, so its last parts stay; the tooltip names all of it.
  let { crumbs, full, onOpen }: { crumbs: PathPart[]; full: string; onOpen: (path: string) => void } = $props();
</script>

<nav class="path" aria-label="Path" title={full}>
  <span class="parts">
    {#each crumbs as crumb, i (`${i}:${crumb.label}`)}
      {#if i > 0}<span class="sep" aria-hidden="true">/</span>{/if}
      {#if crumb.path !== null}
        {@const path = crumb.path}
        <button
          type="button"
          class="crumb"
          class:current={i === crumbs.length - 1}
          aria-current={i === crumbs.length - 1 ? "page" : undefined}
          onclick={() => onOpen(path)}>{crumb.label}</button
        >
      {:else}
        <span class="crumb current" aria-current="page">{crumb.label}</span>
      {/if}
    {/each}
  </span>
</nav>

<style>
  /* Right to left with the parts isolated: the overflow and its ellipsis are on the left. */
  .path {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
    direction: rtl;
    text-align: left;
  }

  .parts {
    direction: ltr;
    unicode-bidi: isolate;
  }

  .crumb {
    font: inherit;
    font-size: 12px;
    color: var(--secondary);
    padding: 0 2px;
    border: 0;
    background: none;
    border-radius: 4px;
  }

  button.crumb:hover {
    color: var(--primary);
  }

  button.crumb:focus-visible {
    outline: 3px solid color-mix(in srgb, var(--accent), transparent 50%);
    outline-offset: -2px;
  }

  .crumb.current {
    color: var(--primary);
    font-weight: 600;
  }

  .sep {
    margin: 0 3px;
    font-size: 12px;
    color: var(--tertiary);
  }
</style>
