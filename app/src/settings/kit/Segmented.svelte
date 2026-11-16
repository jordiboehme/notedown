<script lang="ts" generics="T extends string">
  // The macOS segmented control: one choice of a few.
  let {
    value,
    options,
    label,
    disabled = false,
    onchange,
  }: {
    value: T;
    options: { value: T; label: string }[];
    label: string;
    disabled?: boolean;
    onchange: (value: T) => void;
  } = $props();
</script>

<div class="segmented" role="radiogroup" aria-label={label}>
  {#each options as option (option.value)}
    <button
      type="button"
      role="radio"
      aria-checked={option.value === value}
      {disabled}
      onclick={() => onchange(option.value)}>{option.label}</button
    >
  {/each}
</div>

<style>
  .segmented {
    display: inline-flex;
    padding: 1px;
    gap: 1px;
    border-radius: 6px;
    background: var(--segment-track);
  }

  button {
    font: inherit;
    color: inherit;
    padding: 1px 12px;
    border: none;
    border-radius: 5px;
    background: transparent;
    white-space: nowrap;
  }

  button[aria-checked="true"] {
    background: var(--segment-on);
    box-shadow: 0 0.5px 1.5px rgba(0, 0, 0, 0.25);
  }
</style>
