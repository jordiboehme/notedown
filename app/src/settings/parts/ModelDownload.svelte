<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import {
    MODEL_FAILED_EVENT,
    MODEL_PROGRESS_EVENT,
    MODEL_READY_EVENT,
    failure,
    modelCleanOld,
    modelPause,
    modelPull,
    modelRemove,
    modelState,
    openLink,
    type ModelState,
    type Progress,
  } from "../api";
  import { formatBytes, fraction } from "../format";
  import { licences, loadLicences } from "../modelLicences.svelte";
  import {
    MODEL_TEXT,
    canDownload,
    cleanOldButton,
    cleanOldQuestion,
    filesNote,
    licenceGroups,
    modelJoint,
  } from "../modelText";
  import Row from "../kit/Row.svelte";
  import StatusDot from "../kit/StatusDot.svelte";

  // The rows of the OCR model: its licences (before the download), its state with Download,
  // Pause and Remove, the progress of a download, the size on disk, the folder and the files.
  // Download waits until the licences are on screen. When the model files of Notedown 0.1 are
  // still there, a button offers to remove them after a question; nothing removes them on its own. The OCR page and step 4 of the setup show it;
  // `onchange` tells the page each new state.
  let { onchange }: { onchange?: (model: ModelState) => void } = $props();

  let model = $state<ModelState | null>(null);
  let progress = $state<Progress | null>(null);
  let error = $state<string | null>(null);
  let busy = $state(false);
  let asking = $state(false);
  let askingOld = $state(false);

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

  const groups = $derived(licenceGroups(licences.files ?? []));

  /** A model's page opens in the browser (Rust opens only pages on Hugging Face). */
  function openPage(event: MouseEvent, page: string) {
    event.preventDefault();
    openLink(page).catch((error: unknown) => console.error("notedown: the link did not open", error));
  }

  onMount(() => {
    void check();
    void loadLicences();
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
  const cleanOld = () =>
    act(async () => {
      askingOld = false;
      model = await modelCleanOld();
    });
</script>

<Row title="Model" note={MODEL_TEXT.note}>
  <span class="value">{MODEL_TEXT.size}</span>
</Row>
{#if groups.length > 0}
  <p class="licences">
    Licences: {#each groups as group, g (g)}{#if g > 0}{"; "}{/if}{#each group.models as model, i (model.name)}{modelJoint(
          i,
          group.models.length,
        )}{#if model.page}<a href={model.page} onclick={(event) => openPage(event, model.page!)}>{model.name}</a
          >{:else}{model.name}{/if}{/each} ({group.licensor}): {group.licence}{/each}
  </p>
{:else if licences.error}
  <div class="licences bad">
    <span>{licences.error}</span>
    <button type="button" class="button" onclick={() => void loadLicences()}>Retry</button>
  </div>
{:else}
  <p class="licences">Reading the licences...</p>
{/if}
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
    <button type="button" class="button" disabled={busy || !canDownload(licences)} onclick={download}>Download</button>
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
  {#if model.folder}<Row title="Folder" note={model.folder} path />{/if}
  {#if model.files.length > 0}<Row title="Files" note={filesNote(model.files)} />{/if}
  {#if model.oldFiles}
    <div class="old">
      <button type="button" class="button" disabled={busy} onclick={() => (askingOld = true)}>
        {cleanOldButton(model.oldBytes)}
      </button>
    </div>
    {#if askingOld}
      <div class="confirm" role="alertdialog" aria-labelledby="clean-old-question">
        <p id="clean-old-question">{cleanOldQuestion(model.oldBytes)}</p>
        <div class="answers">
          <button type="button" class="button" onclick={() => (askingOld = false)}>Cancel</button>
          <button type="button" class="button strong" disabled={busy} onclick={cleanOld}>Delete</button>
        </div>
      </div>
    {/if}
  {/if}
{/if}

<style>
  .licences {
    margin: 0;
    padding: 0 10px 9px;
    font-size: 11px;
    color: var(--secondary);
    overflow-wrap: anywhere;
    /* The status row below follows a paragraph, not a row, so this is its separator. */
    border-bottom: 1px solid var(--separator);
  }

  .licences a {
    color: var(--accent);
  }

  .licences.bad {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    color: var(--bad);
  }

  .old {
    display: flex;
    justify-content: flex-end;
    padding: 9px 10px;
    border-top: 1px solid var(--separator);
  }

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
