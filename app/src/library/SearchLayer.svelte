<script lang="ts">
  import type { Snippet } from "svelte";

  // The main area with the search: while a search shows its results, they lie over the view,
  // which stays mounted (a review keeps its place, its zoom and its images) but takes no
  // input. Without a search the view shows alone.
  let { searching, view, results }: { searching: boolean; view: Snippet; results: Snippet } = $props();
</script>

<div class="layer">
  <div class="view" class:covered={searching} inert={searching}>
    {@render view()}
  </div>
  {#if searching}
    <div class="results">
      {@render results()}
    </div>
  {/if}
</div>

<style>
  .layer {
    position: relative;
    flex: 1;
    display: flex;
    min-height: 0;
    min-width: 0;
  }

  .view {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-height: 0;
    min-width: 0;
  }

  /* Hidden but laid out, so its scroll places stay; a page shown alone hides with it. */
  .view.covered {
    visibility: hidden;
  }

  .results {
    position: absolute;
    inset: 0;
    z-index: 11;
    display: flex;
    flex-direction: column;
    background: var(--box);
  }
</style>
