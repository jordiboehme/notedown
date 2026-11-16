<script lang="ts">
  import type { Snippet } from "svelte";

  // One settings page: a grouped form with Roger's margins, scrolled on its own.
  let { children }: { children: Snippet } = $props();
</script>

<div class="form">{@render children()}</div>

<style>
  .form {
    display: flex;
    flex-direction: column;
    gap: 20px;
    padding: 20px 20px 24px;
  }
</style>
