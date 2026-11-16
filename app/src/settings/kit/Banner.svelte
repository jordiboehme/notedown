<script lang="ts">
  import type { Snippet } from "svelte";

  // A notice in a box: an orange warning sign, the message, and an optional action.
  let { message, children }: { message: string; children?: Snippet } = $props();
</script>

<div class="banner" role="note">
  <svg class="sign" viewBox="0 0 24 24" aria-hidden="true">
    <path d="M12 3L22 20H2z" />
    <path class="mark" d="M12 9.5v5 M12 17.2v.01" />
  </svg>
  <p>{message}</p>
  {#if children}{@render children()}{/if}
</div>

<style>
  .banner {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 9px 10px;
  }

  .sign {
    flex: none;
    width: 20px;
    height: 20px;
  }

  .sign path {
    fill: var(--warning);
    stroke: var(--warning);
    stroke-width: 2;
    stroke-linejoin: round;
  }

  .sign .mark {
    fill: none;
    stroke: #fff;
    stroke-linecap: round;
  }

  p {
    flex: 1;
    margin: 0;
  }
</style>
