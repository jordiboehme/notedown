<script lang="ts">
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { LogicalSize } from "@tauri-apps/api/dpi";
  import { listen } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { NOTES_EVENT, type NoteSummary } from "../lib/library-types";
  import { startStatus, status } from "../lib/status.svelte";
  import Panel from "./Panel.svelte";
  import { PANEL_WIDTH, panelHeight } from "./fit";
  import { newNotesView } from "./notes";
  import { cancelQuit, confirmQuit, followQuitRequests, pressQuit, quitQuestion } from "./quit.svelte";

  // The panel window: follows the status and the Quit requests of Rust, keeps the relative
  // times current and fits the window to the content.
  let now = $state(Date.now());
  let content: HTMLDivElement;
  let newNotes = $state<NoteSummary[] | null>(null);
  /** Counts the asks: an older answer that comes late is dropped. */
  let asked = 0;

  function call(command: string, args?: Record<string, unknown>) {
    invoke(command, args).catch((error: unknown) => console.error(`notedown: ${command} failed`, error));
  }

  /** The new notes, asked when the panel opens and on each notes event. */
  function loadNotes() {
    const ask = ++asked;
    invoke<NoteSummary[]>("notes_new")
      .then((notes) => {
        if (ask === asked) newNotes = notes;
      })
      .catch((error: unknown) => console.error("notedown: notes_new failed", error));
  }

  function opened() {
    now = Date.now();
    loadNotes();
  }

  onMount(() => {
    const stops = [startStatus(), followQuitRequests(), listen(NOTES_EVENT, loadNotes)];
    loadNotes();
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
<svelte:window onblur={cancelQuit} onfocus={opened} />

<div bind:this={content}>
  <Panel
    status={status.current}
    {now}
    asking={quitQuestion.open}
    notes={newNotesView(newNotes)}
    onOpenNote={(note) => call("open_library", { note })}
    onMoreNotes={() => call("open_library", { newNotes: true })}
    onSetUp={() => call("open_settings", { page: "setup" })}
    onOpenLibrary={() => call("open_library")}
    onSettings={() => call("open_settings", { page: null })}
    onQuit={() => pressQuit(status.current)}
    onConfirmQuit={confirmQuit}
    onCancelQuit={cancelQuit}
    onPauseMenu={(x, y) => call("panel_pause_menu", { x, y })}
    onResumeOcr={() => call("panel_ocr_resume")}
  />
</div>
