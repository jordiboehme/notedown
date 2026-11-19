<script lang="ts">
  import PathBar from "./PathBar.svelte";
  import SearchField from "./SearchField.svelte";
  import type { LibrarySearch } from "./search.svelte";
  import { viewButtons, type LibraryView, type PathPart } from "./view";

  // The toolbar of the library window: the path of what is shown, the view button that
  // switches the folders between a tree in the sidebar and columns, and the search field that
  // searches the library.
  let {
    view,
    onView,
    search,
    path,
    onOpen,
  }: {
    view: LibraryView;
    onView: (view: LibraryView) => void;
    search: LibrarySearch;
    path: { crumbs: PathPart[]; full: string };
    /** A folder of the path was clicked. */
    onOpen: (path: string) => void;
  } = $props();

  const buttons = $derived(viewButtons(view));
  let field: SearchField | undefined = $state();

  /** Cmd+F. */
  export function focusSearch() {
    field?.focus();
  }
</script>

<div class="toolbar">
  <PathBar crumbs={path.crumbs} full={path.full} {onOpen} />
  <div class="switch" role="group" aria-label="View">
    {#each buttons as button (button.id)}
      <button type="button" class:pressed={button.pressed} aria-pressed={button.pressed} onclick={() => onView(button.id)}>
        {button.label}
      </button>
    {/each}
  </div>
  <SearchField bind:this={field} {search} />
</div>

<style>
  .toolbar {
    flex: none;
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 4px 10px;
    border-bottom: 1px solid var(--separator);
  }

  .switch {
    flex: none;
    display: inline-flex;
    border: 0.5px solid var(--button-rim);
    border-radius: 5px;
    overflow: hidden;
  }

  button {
    font: inherit;
    font-size: 12px;
    color: var(--primary);
    padding: 1px 12px;
    border: 0;
    background: var(--button);
  }

  button + button {
    border-left: 0.5px solid var(--button-rim);
  }

  /* Paper and ink for the shown view. */
  button.pressed {
    color: var(--on-brand);
    background: var(--brand);
  }

  button:focus-visible {
    outline: 3px solid color-mix(in srgb, var(--accent), transparent 50%);
    outline-offset: -2px;
  }
</style>
