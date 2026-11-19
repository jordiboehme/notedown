<script lang="ts">
  import type { Status } from "../lib/api";
  import { panelLines } from "./lines";
  import type { newNotesView } from "./notes";
  import { QUIT_QUESTION } from "./quit.svelte";

  // The menu bar panel as mockup B: the lines, the new notes, the footer. It only shows what it
  // gets; PanelWindow.svelte wires it to Rust. Note titles are text.
  let {
    status,
    now,
    asking,
    notes,
    onOpenNote,
    onMoreNotes,
    onSetUp,
    onOpenLibrary,
    onSettings,
    onQuit,
    onConfirmQuit,
    onCancelQuit,
    onPauseMenu,
    onResumeOcr,
  }: {
    status: Status | null;
    now: number;
    asking: boolean;
    notes: ReturnType<typeof newNotesView>;
    onOpenNote: (path: string) => void;
    onMoreNotes: () => void;
    onSetUp: () => void;
    onOpenLibrary: () => void;
    onSettings: () => void;
    onQuit: () => void;
    onConfirmQuit: () => void;
    onCancelQuit: () => void;
    /** Opens the native Pause OCR menu at `x`, `y` (CSS pixels of the window). */
    onPauseMenu: (x: number, y: number) => void;
    onResumeOcr: () => void;
  } = $props();

  /** The Pause OCR menu opens under the button, as a pull-down menu does. */
  function openPauseMenu(event: MouseEvent) {
    const rect = (event.currentTarget as HTMLElement).getBoundingClientRect();
    onPauseMenu(rect.left, rect.bottom + 2);
  }

  const lines = $derived(status ? panelLines(status, now) : []);
</script>

<div class="panel">
  {#if asking}
    <div class="question" role="alertdialog" aria-labelledby="quit-question">
      <p id="quit-question">{QUIT_QUESTION}</p>
      <div class="answers">
        <button type="button" class="button" onclick={onCancelQuit}>Cancel</button>
        <button type="button" class="button strong" onclick={onConfirmQuit}>Quit</button>
      </div>
    </div>
  {/if}

  {#if status === null}
    <p class="waiting">Looking at the status...</p>
  {:else}
    <ul class="lines">
      {#each lines as line (line.key)}
        <li class="line" data-tone={line.tone} data-attention={line.attention}>
          <span class="dot" aria-hidden="true"></span>
          <div class="body">
            <div class="head">
              <span class="label">{line.label}</span>
              {#if line.when}<span class="when">{line.when}</span>{/if}
            </div>
            <p class="text">{line.text}</p>
            {#if line.more}<p class="more">{line.more}</p>{/if}
            {#if line.action === "setup"}
              <button type="button" class="button strong setup" onclick={onSetUp}>Set up</button>
            {/if}
            {#if line.pause}
              <div class="pause">
                {#if line.pause.action === "resume"}
                  <button type="button" class="button small" onclick={onResumeOcr}>{line.pause.button}</button>
                {:else}
                  <button type="button" class="button small pulldown" aria-haspopup="menu" onclick={openPauseMenu}
                    >{line.pause.button}</button
                  >
                {/if}
                {#if line.pause.change}
                  <button type="button" class="plain small pulldown" aria-haspopup="menu" onclick={openPauseMenu}
                    >Change Pause</button
                  >
                {/if}
              </div>
            {/if}
          </div>
        </li>
      {/each}
    </ul>
  {/if}

  <section class="new-notes" class:none={notes.rows.length === 0} data-slot="new-notes" aria-label="New notes">
    {#if notes.rows.length > 0}
      <h2 class="notes-head">New notes</h2>
      <ul class="notes">
        {#each notes.rows as note (note.key)}
          <li>
            <button type="button" class="note" onclick={() => onOpenNote(note.path)}>
              <span class="note-title">{note.title}</span>
              {#if note.folder}<span class="note-folder">{note.folder}</span>{/if}
            </button>
          </li>
        {/each}
      </ul>
      {#if notes.more}<button type="button" class="plain more-notes" onclick={onMoreNotes}>{notes.more}</button>{/if}
    {/if}
  </section>

  <footer class="footer">
    <button type="button" class="button" onclick={onOpenLibrary}>Open Library</button>
    <span class="spacer"></span>
    <button type="button" class="plain" onclick={onSettings}>Settings</button>
    <button type="button" class="plain" onclick={onQuit}>Quit</button>
  </footer>
</div>

<style>
  /* Paper and ink with the rim of the app icon; the dots in calm colours. */
  .panel {
    --sheet: #f7f5ef;
    --fg: var(--color-ink);
    --muted: #6b665c;
    --rule: #e1ddd1;
    --rim: var(--color-rim);
    --button: #ffffff;
    --strong-bg: var(--color-ink);
    --strong-fg: var(--color-paper);
    --question: var(--color-paper);
    --ok: #2f7d4f;
    --wait: #b7791f;
    --problem: #b3261e;
    --off: #a9a497;

    background: var(--sheet);
    color: var(--fg);
    user-select: none;
    cursor: default;
  }

  @media (prefers-color-scheme: dark) {
    .panel {
      --sheet: var(--color-ink);
      --fg: var(--color-paper);
      --muted: #a39e92;
      --rule: #34322e;
      --rim: #4a473f;
      --button: #2c2b28;
      --strong-bg: var(--color-paper);
      --strong-fg: var(--color-ink);
      --question: #2a2925;
      --ok: #5cb27e;
      --wait: #d9a441;
      --problem: #e5675f;
      --off: #66625a;
    }
  }

  .question {
    padding: 12px 14px;
    background: var(--question);
    border-bottom: 1px solid var(--rim);
  }

  .question p {
    margin: 0 0 10px;
  }

  .answers {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }

  .waiting {
    margin: 0;
    padding: 12px 14px;
    color: var(--muted);
  }

  .lines {
    list-style: none;
    margin: 0;
    padding: 4px 0;
  }

  .line {
    display: flex;
    gap: 8px;
    padding: 7px 14px;
  }

  .line + .line {
    border-top: 1px solid var(--rule);
  }

  .dot {
    flex: none;
    width: 8px;
    height: 8px;
    margin-top: 5px;
    border-radius: 50%;
    background: var(--off);
  }

  .line[data-tone="ok"] .dot {
    background: var(--ok);
  }

  .line[data-tone="wait"] .dot {
    background: var(--wait);
  }

  .line[data-tone="problem"] .dot {
    background: var(--problem);
  }

  .body {
    flex: 1;
    min-width: 0;
  }

  .head {
    display: flex;
    justify-content: space-between;
    gap: 8px;
  }

  .label {
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .when {
    flex: none;
    color: var(--muted);
    font-size: 12px;
    font-variant-numeric: tabular-nums;
  }

  .text {
    margin: 0;
    overflow-wrap: anywhere;
  }

  .line[data-attention="true"] .text {
    color: var(--problem);
  }

  .more {
    margin: 0;
    color: var(--muted);
    font-size: 12px;
  }

  .setup {
    margin-top: 6px;
  }

  .pause {
    display: flex;
    gap: 4px;
    margin-top: 5px;
  }

  .small {
    padding: 1px 8px;
    font-size: 12px;
  }

  /* A pull-down button: a small chevron after the text, as in a Mac menu button. */
  .pulldown::after {
    content: "";
    display: inline-block;
    width: 4px;
    height: 4px;
    margin: 0 1px 2px 6px;
    border-right: 1.5px solid currentColor;
    border-bottom: 1.5px solid currentColor;
    transform: rotate(45deg);
    vertical-align: middle;
  }

  .new-notes.none {
    display: none;
  }

  .new-notes {
    padding: 6px 0 8px;
    border-top: 1px solid var(--rule);
  }

  .notes-head {
    margin: 0;
    padding: 2px 14px 4px;
    font-size: 11px;
    font-weight: 600;
    color: var(--muted);
  }

  .notes {
    list-style: none;
    margin: 0;
    padding: 0;
  }

  .note {
    display: flex;
    align-items: baseline;
    gap: 8px;
    width: 100%;
    padding: 3px 14px;
    font: inherit;
    color: inherit;
    text-align: left;
    background: transparent;
    border: none;
  }

  .note:hover {
    background: var(--rule);
  }

  .note-title {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .note-folder {
    flex: none;
    max-width: 45%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--muted);
    font-size: 12px;
  }

  .more-notes {
    margin: 2px 0 0 4px;
  }

  .footer {
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 8px 10px 8px 14px;
    border-top: 1px solid var(--rim);
  }

  .spacer {
    flex: 1;
  }

  .button,
  .plain {
    font: inherit;
    color: inherit;
    border-radius: 6px;
    padding: 3px 10px;
  }

  .button {
    background: var(--button);
    border: 1px solid var(--rim);
  }

  .button.strong {
    background: var(--strong-bg);
    color: var(--strong-fg);
    border-color: var(--strong-bg);
  }

  .plain {
    background: transparent;
    border: 1px solid transparent;
    color: var(--muted);
  }

  .plain:hover {
    color: var(--fg);
    background: var(--rule);
  }

  button:focus-visible {
    outline: 2px solid var(--wait);
    outline-offset: 1px;
  }
</style>
