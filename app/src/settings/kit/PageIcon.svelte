<script lang="ts">
  import type { Page } from "../pages";

  // A white line icon on a 20 x 20 square with a 5 px radius, in the page's colour with a
  // light top-to-bottom gradient (SwiftUI's `.gradient`).
  let { page }: { page: Page } = $props();
</script>

<span class="icon" style:--tint={page.color} aria-hidden="true">
  <svg viewBox="0 0 24 24"><path d={page.icon} /></svg>
</span>

<style>
  .icon {
    display: inline-flex;
    flex: none;
    align-items: center;
    justify-content: center;
    width: 20px;
    height: 20px;
    border-radius: 5px;
    background: linear-gradient(to bottom, color-mix(in srgb, var(--tint), #fff 15%), var(--tint));
  }

  svg {
    width: 14px;
    height: 14px;
    fill: none;
    stroke: #fff;
    stroke-width: 2;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
</style>
