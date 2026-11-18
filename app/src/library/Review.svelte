<script lang="ts">
  import { onMount, tick } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { NOTES_EVENT } from "../lib/library-types";
  import { ReviewSession } from "./session.svelte";
  import { concernsNote, noteTitle, reviewNotice, reviewPages } from "./review";
  import ReviewPages from "./ReviewPages.svelte";
  import PageViewer from "./PageViewer.svelte";
  import { ACTUAL, ZOOMS, zoomIn, zoomKey, zoomOut } from "./zoom";

  // The review of one note: opened when it mounts (that marks it seen), read again when the
  // store changed (new page text shows while it is open), closed in Rust when it goes. The
  // window keys it by the path, so another note is another review.
  let { path, onBack }: { path: string; onBack: () => void } = $props();

  // svelte-ignore state_referenced_locally
  const session = new ReviewSession(path);
  const rows = $derived(session.view ? reviewPages(session.view, session.ratios) : []);
  const notice = $derived(session.view ? reviewNotice(session.view) : null);
  /** The zoom of the pages side: 1 is fit width. */
  let zoom = $state(ACTUAL);

  function zoomBy(step: "in" | "out" | "actual") {
    zoom = step === "in" ? zoomIn(zoom) : step === "out" ? zoomOut(zoom) : ACTUAL;
  }

  let pages = $state<ReturnType<typeof ReviewPages>>();

  /** Ends the large view of one page: the pages side shows the page that was shown last. */
  async function closeViewer() {
    const last = session.focused;
    session.focus(null);
    if (last === null) return;
    await tick();
    pages?.showPage(last);
  }

  function key(event: KeyboardEvent) {
    // The large view of one page has its own zoom.
    if (session.focused !== null) return;
    const step = zoomKey(event);
    if (!step || !session.view) return;
    event.preventDefault();
    zoomBy(step);
  }

  onMount(() => {
    void session.open();
    // Only a store change of this note (the OCR wrote one of its pages) reads it again.
    const stop = listen(NOTES_EVENT, (event) => {
      if (concernsNote(event.payload, path)) void session.reload();
    });
    return () => {
      void stop.then((unlisten) => unlisten());
      session.close();
    };
  });
</script>

<svelte:window onfocus={() => session.refresh()} onkeydown={key} />

<header class="head">
  <button type="button" class="back" onclick={onBack}>Back</button>
  <h1 class="title">{session.view?.title ?? noteTitle(path)}</h1>
  {#if session.view && rows.length > 0}
    <div class="zoom" role="group" aria-label="Zoom">
      <button type="button" class="back" disabled={zoom === ZOOMS[0]} onclick={() => zoomBy("out")}>Zoom Out</button>
      <button type="button" class="back" disabled={zoom === ACTUAL} onclick={() => zoomBy("actual")}>Actual Size</button>
      <button type="button" class="back" disabled={zoom === ZOOMS[ZOOMS.length - 1]} onclick={() => zoomBy("in")}>Zoom In</button>
    </div>
  {/if}
</header>
{#if session.stale}<p class="notice">{session.stale}</p>{/if}
{#if notice}<p class="notice">{notice}</p>{/if}

{#if session.error}
  <div class="failed">
    <p class="note">{session.error}</p>
    <button type="button" class="back" onclick={() => void session.reload()}>Try Again</button>
  </div>
{:else if !session.view}
  <p class="note">Opening the note...</p>
{:else if rows.length === 0}
  <p class="note">This note has no pages.</p>
{:else}
  <ReviewPages
    bind:this={pages}
    {rows}
    {zoom}
    images={session.images}
    onSeen={(changes) => session.seenMany(changes)}
    onMeasured={(index, width, height) => session.measured(index, width, height)}
    onOpen={(index) => session.focus(index)}
  />
  {#if session.focused !== null && rows[session.focused]}
    <PageViewer
      row={rows[session.focused]}
      image={session.images[session.focused]}
      count={rows.length}
      onStep={(index) => session.focus(index)}
      onClose={() => void closeViewer()}
    />
  {/if}
{/if}

<style>
  .head {
    flex: none;
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 6px 14px;
    border-bottom: 1px solid var(--separator);
  }

  .back {
    font: inherit;
    padding: 2px 10px;
    border-radius: 6px;
    border: 1px solid var(--button-rim);
    background: var(--button);
    color: inherit;
  }

  .back:disabled {
    opacity: 0.5;
  }

  .zoom {
    flex: none;
    display: flex;
    gap: 6px;
    margin-left: auto;
  }

  .title {
    min-width: 0;
    margin: 0;
    font-size: 13px;
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .notice {
    flex: none;
    margin: 0;
    padding: 6px 14px;
    color: var(--secondary);
    border-bottom: 1px solid var(--separator);
  }

  .failed {
    display: flex;
    align-items: center;
    gap: 10px;
    padding-right: 14px;
  }

  .note {
    margin: 0;
    padding: 10px 14px;
    color: var(--secondary);
  }
</style>
