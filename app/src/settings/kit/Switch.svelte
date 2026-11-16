<script lang="ts">
  // The macOS switch: a pill that slides, the accent colour when on.
  let {
    checked,
    label,
    disabled = false,
    onchange,
  }: { checked: boolean; label: string; disabled?: boolean; onchange: (on: boolean) => void } = $props();
</script>

<button
  type="button"
  class="switch"
  role="switch"
  aria-checked={checked}
  aria-label={label}
  {disabled}
  onclick={() => onchange(!checked)}
>
  <span class="knob"></span>
</button>

<style>
  .switch {
    position: relative;
    width: 32px;
    height: 18px;
    padding: 0;
    border: none;
    border-radius: 9px;
    background: var(--switch-off);
    transition: background 0.15s;
  }

  .switch[aria-checked="true"] {
    background: var(--accent);
  }

  .switch:disabled {
    opacity: 0.5;
  }

  .knob {
    position: absolute;
    top: 1px;
    left: 1px;
    width: 16px;
    height: 16px;
    border-radius: 50%;
    background: #fff;
    box-shadow: 0 1px 2px rgba(0, 0, 0, 0.3);
    transition: transform 0.15s;
  }

  .switch[aria-checked="true"] .knob {
    transform: translateX(14px);
  }
</style>
