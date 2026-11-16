<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";

  // A placeholder until the settings window is built. Rust names the page to open: at first
  // through window.__NOTEDOWN_PAGE__, later through the settings-page event.
  const start = (window as unknown as { __NOTEDOWN_PAGE__?: string | null }).__NOTEDOWN_PAGE__;
  let page = $state<string | null>(start ?? null);

  onMount(() => {
    const unlisten = listen<string>("settings-page", (event) => {
      page = event.payload;
    });
    return () => {
      void unlisten.then((stop) => stop());
    };
  });
</script>

<main class="flex h-full flex-col items-center justify-center gap-2 bg-paper text-ink dark:bg-ink dark:text-paper">
  <h1 class="text-xl font-semibold">Settings</h1>
  {#if page}<p class="text-sm">Page: {page}</p>{/if}
</main>
