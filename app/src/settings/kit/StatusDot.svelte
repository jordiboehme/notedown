<script lang="ts">
  // An 8 x 8 dot and a short secondary text (Roger's SettingsStatus).
  let { level, text }: { level: "good" | "warning" | "bad" | "unknown"; text: string } = $props();
</script>

<span class="status" data-level={level}><span class="dot" aria-hidden="true"></span>{text}</span>

<style>
  .status {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    color: var(--secondary);
    white-space: nowrap;
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--unknown);
  }

  [data-level="good"] .dot {
    background: var(--good);
  }

  [data-level="warning"] .dot {
    background: var(--warning);
  }

  [data-level="bad"] .dot {
    background: var(--bad);
  }
</style>
