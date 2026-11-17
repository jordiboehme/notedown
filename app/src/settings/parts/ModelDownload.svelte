<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import {
    MODEL_FAILED_EVENT,
    MODEL_PROGRESS_EVENT,
    MODEL_READY_EVENT,
    failure,
    modelPause,
    modelPull,
    modelRemove,
    modelState,
    type ModelState,
    type Progress,
  } from "../api";
  import { formatBytes, fraction } from "../format";
  import Row from "../kit/Row.svelte";
  import StatusDot from "../kit/StatusDot.svelte";

  // The rows of the OCR model: its state with Download, Pause and Remove, the progress of a
  // download, the size on disk and the file. The OCR page and step 4 of the setup show it;
  // `onchange` tells the page each new state.
  let { onchange }: { onchange?: (model: ModelState) => void } = $props();

  let model = $state<ModelState | null>(null);
  let progress = $state<Progress | null>(null);
  let error = $state<string | null>(null);
  let busy = $state(false);
  let asking = $state(false);

  const pulling = $derived(model?.pulling ?? false);

  $effect(() => {
    if (model) onchange?.($state.snapshot(model) as ModelState);
  });

  async function check() {
    try {
      model = await modelState();
      progress = model.progress;
    } catch (e) {
      error = failure(e);
    }
  }

  onMount(() => {
    void check();
    const stops = [
      listen<Progress>(MODEL_PROGRESS_EVENT, (event) => {
        progress = event.payload;
        if (model) model.pulling = true;
      }),
      listen<ModelState>(MODEL_READY_EVENT, (event) => {
        model = event.payload;
        progress = null;
        error = null;
      }),
      listen<{ text: string }>(MODEL_FAILED_EVENT, (event) => {
        error = event.payload.text;
        progress = null;
        void check();
      }),
    ];
    return () => {
      for (const stop of stops) void stop.then((unlisten) => unlisten());
    };
  });

  async function act(run: () => Promise<unknown>) {
    busy = true;
    error = null;
    try {
      await run();
    } catch (e) {
      error = failure(e);
    } finally {
      busy = false;
    }
  }

  const download = () =>
    act(async () => {
      await modelPull();
      if (model) model.pulling = true;
    });
  const pause = () =>
    act(async () => {
      model = await modelPause();
      progress = null;
    });
  const remove = () =>
    act(async () => {
      asking = false;
      model = await modelRemove();
    });
</script>

<Row title="Model" note="Chandra OCR 2 runs on your Mac. Your notes never leave it.">
  <span class="value">About 3.4 GB</span>
</Row>
<Row title="Status" note={error ?? model?.problem ?? null} tone="bad">
  {#if model === null}
    <span class="value">Checking...</span>
  {:else if pulling}
    <progress class="bar" max="1" value={progress ? fraction(progress.done, progress.total) : 0}></progress>
    <span class="value">
      {progress ? `${formatBytes(progress.done)} of ${formatBytes(progress.total)}` : "Starting..."}
    </span>
    <button type="button" class="button" disabled={busy} onclick={pause}>Pause</button>
  {:else if model.installed}
    <StatusDot level="good" text="Downloaded" />
    <button type="button" class="button danger" disabled={busy} onclick={() => (asking = true)}>Remove...</button>
  {:else}
    <StatusDot
      level={model.sizeBytes > 0 ? "warning" : "bad"}
      text={model.sizeBytes > 0 ? "Partly downloaded" : "Not downloaded"}
    />
    <button type="button" class="button" disabled={busy} onclick={download}>Download</button>
  {/if}
</Row>
{#if asking}
  <div class="confirm" role="alertdialog" aria-labelledby="remove-question">
    <p id="remove-question">
      Remove the OCR model? Notedown deletes it from your Mac. An OCR that runs now keeps the model it loaded
      until it starts again. After that, OCR waits until you download the model again.
    </p>
    <div class="answers">
      <button type="button" class="button" onclick={() => (asking = false)}>Cancel</button>
      <button type="button" class="button strong" disabled={busy} onclick={remove}>Remove</button>
    </div>
  </div>
{/if}
{#if model}
  <Row title="Size on disk">
    <span class="value">{formatBytes(model.sizeBytes)}</span>
  </Row>
  {#if model.path}<Row title="File" note={model.path} path />{/if}
{/if}

<style>
  .bar {
    width: 120px;
    accent-color: var(--brand);
  }

  .confirm {
    padding: 10px;
    border-top: 1px solid var(--separator);
  }

  .confirm p {
    margin: 0 0 8px;
  }

  .answers {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }
</style>
