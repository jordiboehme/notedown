<script lang="ts">
  import type { Job } from "../lib/library-types";
  import type { Ask } from "../lib/library.svelte";
  import { askView, type AskChoice, type JobAction, type JobGroup } from "./actions";

  // The bottom of the window: what did not work, the questions of a drop, and a row per job
  // with its progress, its question and its buttons. Done rows go after a while.
  let {
    groups,
    asks,
    notice,
    onAction,
    onRetryAll,
    onAnswer,
    onNoticeSeen,
  }: {
    groups: JobGroup[];
    asks: Ask[];
    notice: string | null;
    onAction: (job: Job, action: JobAction | "cancelAll") => void;
    onRetryAll: (ids: number[]) => void;
    onAnswer: (dropId: number, choice: AskChoice) => void;
    onNoticeSeen: () => void;
  } = $props();

  const question = $derived(asks.length > 0 ? { ask: asks[0], view: askView(asks[0].ask) } : null);
</script>

{#if notice || question || groups.length > 0}
  <section class="jobs" aria-label="Jobs">
    {#if notice}
      <div class="line notice" role="alert">
        <span class="text">{notice}</span>
        <button type="button" class="button" onclick={onNoticeSeen}>OK</button>
      </div>
    {/if}
    {#if question}
      <div class="line ask" role="alertdialog" aria-label="Upload">
        <span class="text">{question.view.text}</span>
        {#each question.view.buttons as button (button.id)}
          <button type="button" class="button" onclick={() => onAnswer(question.ask.dropId, button.id)}>{button.label}</button>
        {/each}
      </div>
    {/if}
    <div class="list">
      {#each groups as group (group.rows[0].id)}
        <div class="group">
          {#if group.title || group.cancelAll}
            <div class="line head">
              <span class="text">{group.title ?? ""}</span>
              {#if group.retryAll.length > 0}
                <button type="button" class="button" onclick={() => onRetryAll(group.retryAll)}>Retry all</button>
              {/if}
              {#if group.cancelAll}
                <button type="button" class="button" onclick={() => onAction(group.rows[0].job, "cancelAll")}>Cancel all</button>
              {/if}
            </div>
          {/if}
          {#each group.rows as row (row.id)}
            <div class="line row" class:failed={row.failed} class:asking={row.asking}>
              <span class="label" title={row.label}>{row.label}</span>
              {#if row.progress !== null}
                <progress class="bar" max="1" value={row.progress}></progress>
              {/if}
              <span class="text state">{row.text}</span>
              {#each row.actions as action (action.id)}
                <button type="button" class="button" onclick={() => onAction(row.job, action.id)}>{action.label}</button>
              {/each}
            </div>
          {/each}
          {#if group.more > 0}
            <div class="line head">
              <span class="text">{group.more.toLocaleString("en-US")} more waiting</span>
            </div>
          {/if}
        </div>
      {/each}
    </div>
  </section>
{/if}

<style>
  .jobs {
    flex: none;
    max-height: 30%;
    display: flex;
    flex-direction: column;
    border-top: 1px solid var(--separator);
    background: var(--window);
  }

  .list {
    overflow-y: auto;
  }

  .line {
    display: flex;
    align-items: center;
    gap: 8px;
    min-height: 26px;
    padding: 2px 14px;
    box-sizing: border-box;
  }

  .group + .group {
    border-top: 1px solid var(--separator);
  }

  .head {
    color: var(--secondary);
    font-size: 11px;
  }

  .label {
    flex: 0 1 40%;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .text {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .state {
    color: var(--secondary);
  }

  .failed .state,
  .notice .text {
    color: var(--bad);
  }

  .asking .state,
  .ask .text {
    color: var(--primary);
    font-weight: 600;
  }

  .notice,
  .ask {
    border-bottom: 1px solid var(--separator);
  }

  .bar {
    flex: none;
    width: 120px;
    height: 6px;
    accent-color: var(--brand);
  }
</style>
