<script lang="ts">
  import type { LibraryStateView } from "../lib/library-types";
  import { stateView } from "./view";

  // What the window says while the library cannot be read, with the button that opens Settings
  // at the page the state names.
  let {
    view,
    onSettings,
    compact = false,
  }: { view: LibraryStateView | null; onSettings: (page: string | null) => void; compact?: boolean } = $props();

  const notice = $derived(stateView(view));
</script>

{#if notice}
  <div class="notice" class:compact role="status">
    <p>{notice.text}</p>
    {#if notice.action}
      <button type="button" class="button" onclick={() => onSettings(notice.page)}>{notice.action}</button>
    {/if}
  </div>
{/if}

<style>
  .notice {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 10px;
    margin: auto;
    padding: 24px;
    max-width: 320px;
    text-align: center;
    color: var(--secondary);
  }

  .notice.compact {
    align-items: flex-start;
    margin: 0;
    padding: 4px 14px 8px;
    text-align: left;
  }

  p {
    margin: 0;
  }
</style>
