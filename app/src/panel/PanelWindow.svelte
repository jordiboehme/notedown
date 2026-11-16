<script lang="ts">
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { LogicalSize } from "@tauri-apps/api/dpi";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { startStatus, status } from "../lib/status.svelte";
  import Panel from "./Panel.svelte";
  import { PANEL_WIDTH, panelHeight } from "./fit";
  import { cancelQuit, confirmQuit, followQuitRequests, pressQuit, quitQuestion } from "./quit.svelte";

  // The panel window: follows the status and the Quit requests of Rust, keeps the relative
  // times current and fits the window to the content.
  let now = $state(Date.now());
  let content: HTMLDivElement;

  function call(command: string, args?: Record<string, unknown>) {
    invoke(command, args).catch((error: unknown) => console.error(`notedown: ${command} failed`, error));
  }

  onMount(() => {
    const stops = [startStatus(), followQuitRequests()];
    const clock = setInterval(() => (now = Date.now()), 30_000);
    let height = 0;
    const fit = new ResizeObserver(() => {
      const next = panelHeight(content.getBoundingClientRect().height);
      if (next === height) return;
      height = next;
      getCurrentWindow()
        .setSize(new LogicalSize(PANEL_WIDTH, next))
        .catch((error: unknown) => console.error("notedown: the panel could not be sized", error));
    });
    fit.observe(content);
    return () => {
      fit.disconnect();
      clearInterval(clock);
      for (const stop of stops) void stop.then((unlisten) => unlisten());
    };
  });
</script>

<!-- A panel that loses the focus hides; the question does not wait for the next opening. -->
<svelte:window onblur={cancelQuit} onfocus={() => (now = Date.now())} />

<div bind:this={content}>
  <Panel
    status={status.current}
    {now}
    asking={quitQuestion.open}
    onSetUp={() => call("open_settings", { page: "setup" })}
    onOpenLibrary={() => call("open_library")}
    onSettings={() => call("open_settings", { page: null })}
    onQuit={() => pressQuit(status.current)}
    onConfirmQuit={confirmQuit}
    onCancelQuit={cancelQuit}
  />
</div>
