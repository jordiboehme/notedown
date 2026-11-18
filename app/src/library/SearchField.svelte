<script lang="ts">
  import type { LibrarySearch } from "./search.svelte";

  // The search field of the toolbar. Escape empties it, which gives back the view that was
  // shown. The window focuses it on Cmd+F through `focus`.
  let { search }: { search: LibrarySearch } = $props();

  let input: HTMLInputElement | undefined = $state();

  export function focus() {
    input?.focus();
    input?.select();
  }

  function keydown(event: KeyboardEvent) {
    if (event.key !== "Escape") return;
    event.preventDefault();
    search.clear();
  }
</script>

<input
  bind:this={input}
  class="field"
  type="search"
  placeholder="Search"
  aria-label="Search"
  autocomplete="off"
  spellcheck="false"
  value={search.text}
  oninput={(event) => search.type(event.currentTarget.value)}
  onkeydown={keydown}
/>

<style>
  .field {
    width: 200px;
    font: inherit;
    font-size: 12px;
    color: var(--primary);
    padding: 2px 8px;
    border: 0.5px solid var(--button-rim);
    border-radius: 5px;
    background: var(--field);
    user-select: text;
  }

  .field:focus-visible {
    outline: 3px solid color-mix(in srgb, var(--accent), transparent 50%);
    outline-offset: 0;
  }
</style>
