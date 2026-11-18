<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { NOTES_EVENT } from "../lib/library-types";
  import { ReviewSession } from "./session.svelte";
  import { noteTitle, reviewNotice, reviewPages } from "./review";
  import ReviewPages from "./ReviewPages.svelte";

  // The review of one note: opened when it mounts (that marks it seen), read again when the
  // store changed (new page text shows while it is open), closed in Rust when it goes. The
  // window keys it by the path, so another note is another review.
  let { path, onBack }: { path: string; onBack: () => void } = $props();

  // svelte-ignore state_referenced_locally
  const session = new ReviewSession(path);
  const rows = $derived(session.view ? reviewPages(session.view, session.ratios) : []);
  const notice = $derived(session.view ? reviewNotice(session.view) : null);

  onMount(() => {
    void session.open();
    const stop = listen(NOTES_EVENT, () => void session.reload());
    return () => {
      void stop.then((unlisten) => unlisten());
      session.close();
    };
  });
</script>

<svelte:window onfocus={() => session.refresh()} />

<header class="head">
  <button type="button" class="back" onclick={onBack}>Back</button>
  <h1 class="title">{session.view?.title ?? noteTitle(path)}</h1>
</header>
{#if notice}<p class="notice">{notice}</p>{/if}

{#if session.error}
  <p class="note">{session.error}</p>
{:else if !session.view}
  <p class="note">Opening the note...</p>
{:else if rows.length === 0}
  <p class="note">This note has no pages.</p>
{:else}
  <ReviewPages
    {rows}
    images={session.images}
    onSeen={(changes) => session.seenMany(changes)}
    onMeasured={(index, width, height) => session.measured(index, width, height)}
  />
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

  .title {
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

  .note {
    margin: 0;
    padding: 10px 14px;
    color: var(--secondary);
  }
</style>
