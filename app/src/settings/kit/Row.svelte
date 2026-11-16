<script lang="ts">
  import type { Snippet } from "svelte";

  // Title with one explaining line on the left, the control on the right (Roger's
  // SettingsRow). `path` notes stay on one line, cut in the middle, the whole text as a tooltip.
  let {
    title,
    note,
    tone = "secondary",
    path = false,
    children,
  }: {
    title: string;
    note?: string | null;
    tone?: "secondary" | "good" | "bad";
    path?: boolean;
    children?: Snippet;
  } = $props();

  // The middle cut: the start and the end of the path stay readable.
  const shown = $derived(
    path && note && note.length > 60 ? `${note.slice(0, 24)}...${note.slice(-33)}` : note,
  );
</script>

<div class="row">
  <div class="label">
    <span class="title">{title}</span>
    {#if note}
      <span class="note" data-tone={tone} class:path title={path ? note : undefined}>{shown}</span>
    {/if}
  </div>
  {#if children}<div class="control">{@render children()}</div>{/if}
</div>

<style>
  .row {
    display: flex;
    align-items: center;
    gap: 16px;
    min-height: 22px;
    padding: 9px 10px;
  }

  :global(.row) + .row {
    border-top: 1px solid var(--separator);
  }

  .label {
    display: flex;
    flex: 1;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }

  .note {
    font-size: 11px;
    color: var(--secondary);
    overflow-wrap: anywhere;
  }

  .note.path {
    white-space: nowrap;
    overflow: hidden;
  }

  .note[data-tone="good"] {
    color: var(--good);
  }

  .note[data-tone="bad"] {
    color: var(--bad);
  }

  .control {
    display: flex;
    flex: none;
    align-items: center;
    gap: 8px;
    max-width: 60%;
  }
</style>
