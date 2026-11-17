<script lang="ts">
  import type { Status } from "../lib/api";
  import { panelLines } from "../panel/lines";

  // The status line on top of the library: the lines of the panel in one row.
  let { status, now, onSetUp }: { status: Status | null; now: number; onSetUp: () => void } = $props();

  const lines = $derived(status ? panelLines(status, now) : []);
</script>

<header class="status" aria-label="Status">
  {#if status === null}
    <span class="muted">Looking at the status...</span>
  {:else}
    {#each lines as line (line.key)}
      <span class="line" data-tone={line.tone} title={line.more ?? undefined}>
        <span class="dot" aria-hidden="true"></span>
        <span class="label">{line.label}</span>
        <span class="text">{line.text}{line.when ? `, ${line.when}` : ""}</span>
        {#if line.action === "setup"}
          <button type="button" class="button" onclick={onSetUp}>Set up</button>
        {/if}
      </span>
    {/each}
  {/if}
</header>

<style>
  .status {
    flex: none;
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 4px 18px;
    padding: 6px 14px;
    border-bottom: 1px solid var(--separator);
    background: var(--window);
  }

  .line {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    min-width: 0;
  }

  .dot {
    flex: none;
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--unknown);
  }

  .line[data-tone="ok"] .dot {
    background: var(--good);
  }

  .line[data-tone="wait"] .dot {
    background: var(--warning);
  }

  .line[data-tone="problem"] .dot {
    background: var(--bad);
  }

  .label {
    font-weight: 600;
  }

  .text,
  .muted {
    color: var(--secondary);
  }
</style>
