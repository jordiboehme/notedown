<script lang="ts">
  import type { Snippet } from "svelte";

  // A heading in small secondary text, the rows in a rounded box, an optional footer.
  let { title, footer, children }: { title?: string; footer?: string; children: Snippet } = $props();
</script>

<section class="section">
  {#if title}<h2 class="header">{title}</h2>{/if}
  <div class="box">{@render children()}</div>
  {#if footer}<p class="footer">{footer}</p>{/if}
</section>

<style>
  .header {
    margin: 0 0 6px 10px;
    font-size: 12px;
    font-weight: 600;
    color: var(--secondary);
  }

  .box {
    background: var(--box);
    border: 1px solid var(--box-rim);
    border-radius: 10px;
    overflow: hidden;
  }

  .footer {
    margin: 6px 10px 0;
    font-size: 11px;
    color: var(--secondary);
  }
</style>
