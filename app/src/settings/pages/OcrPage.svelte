<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { relativeTime } from "../../lib/format";
  import { status } from "../../lib/status.svelte";
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
  import Form from "../kit/Form.svelte";
  import Row from "../kit/Row.svelte";
  import Section from "../kit/Section.svelte";
  import StatusDot from "../kit/StatusDot.svelte";

  // Settings > OCR: the queue, the page in work, the last error, and the model with Download,
  // Pause and Remove.
  let { now }: { now: number } = $props();

  let model = $state<ModelState | null>(null);
  let progress = $state<Progress | null>(null);
  let error = $state<string | null>(null);
  let busy = $state(false);
  let asking = $state(false);

  const ocr = $derived(status.current?.ocr ?? null);
  const pulling = $derived(model?.pulling ?? false);

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

<Form>
  <Section title="Queue">
    {#if ocr === null}
      <Row title="Status" note="Looking at the status..." />
    {:else}
      <Row title="Status" note={ocr.line} />
      {#if ocr.inWork}
        <Row title="Page in work" note={ocr.inWork.path}>
          <span class="value">Page {ocr.inWork.page} of {ocr.inWork.pages}</span>
        </Row>
      {/if}
      {#if ocr.lastError}
        <Row title="Last error" note={ocr.lastError.text} tone="bad">
          <span class="value">{relativeTime(ocr.lastError.atMs, now)}</span>
        </Row>
      {/if}
    {/if}
  </Section>

  <Section title="Model">
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
          Remove the OCR model? Notedown deletes it from your Mac. OCR stops until you download it again.
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
  </Section>
</Form>

<style>
  .bar {
    width: 120px;
    accent-color: var(--accent);
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
