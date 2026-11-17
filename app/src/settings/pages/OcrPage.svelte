<script lang="ts">
  import { open } from "@tauri-apps/plugin-dialog";
  import { relativeTime } from "../../lib/format";
  import { status } from "../../lib/status.svelte";
  import { failure, markdownDir, setMarkdownDir, setModelIdle, showMarkdownDir } from "../api";
  import { config } from "../config.svelte";
  import { markdownRead, markdownRow } from "../markdown";
  import { MODEL_IDLE, modelIdleMinutes, modelIdleOptions, modelIdleValue } from "../modelIdle";
  import Form from "../kit/Form.svelte";
  import Row from "../kit/Row.svelte";
  import Section from "../kit/Section.svelte";
  import Segmented from "../kit/Segmented.svelte";
  import ModelDownload from "../parts/ModelDownload.svelte";

  // Settings > OCR: the queue, the page in work, the last error, the Markdown folder with
  // Choose... and Show in Finder, and the model with Download, Pause and Remove, and how long
  // the model stays loaded without new pages.
  let { now }: { now: number } = $props();

  const ocr = $derived(status.current?.ocr ?? null);

  let markdownPath = $state<string | null | undefined>(undefined);
  let markdownError = $state<string | null>(null);
  let markdownBusy = $state(false);
  const markdown = $derived(
    markdownRow({
      path: markdownPath,
      error: markdownError,
      busy: markdownBusy,
      pipeline: status.current?.pipeline.state ?? null,
    }),
  );

  async function readMarkdownDir() {
    const answer = await markdownDir().then(
      (path) => ({ path }),
      (e: unknown) => ({ error: failure(e) }),
    );
    const read = markdownRead(
      { path: markdownPath, error: markdownError, busy: markdownBusy, pipeline: null },
      answer,
    );
    markdownPath = read.path;
    markdownError = read.error;
  }

  // The folder belongs to the account: read it again when the account changes.
  $effect(() => {
    void config.current?.account;
    void readMarkdownDir();
  });

  async function chooseMarkdownDir() {
    markdownError = null;
    const picked = await open({
      directory: true,
      multiple: false,
      defaultPath: markdownPath ?? undefined,
      title: "Choose the Markdown folder",
    }).catch((e: unknown) => {
      markdownError = failure(e);
      return null;
    });
    if (typeof picked !== "string") return;
    markdownBusy = true;
    try {
      markdownPath = await setMarkdownDir(picked);
    } catch (e) {
      markdownError = failure(e);
    } finally {
      markdownBusy = false;
    }
  }

  let idleError = $state<string | null>(null);
  let idleBusy = $state(false);

  async function chooseModelIdle(value: string) {
    idleBusy = true;
    try {
      config.current = await setModelIdle(modelIdleMinutes(value));
      idleError = null;
    } catch (e) {
      idleError = failure(e);
    } finally {
      idleBusy = false;
    }
  }

  async function showMarkdown() {
    markdownError = null;
    try {
      await showMarkdownDir();
    } catch (e) {
      markdownError = failure(e);
    }
  }
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

  <Section title="Markdown">
    <Row title={markdown.title} note={markdown.note ?? markdown.explanation} tone={markdown.tone}>
      <button type="button" class="button" disabled={!markdown.choose.enabled} onclick={chooseMarkdownDir}>
        {markdown.choose.label}
      </button>
      <button type="button" class="button" disabled={!markdown.show.enabled} onclick={showMarkdown}>
        {markdown.show.label}
      </button>
    </Row>
    {#if markdown.path}<Row title="Folder" note={markdown.path} path />{/if}
  </Section>

  <Section title="Model">
    <ModelDownload />
    <Row title={MODEL_IDLE.title} note={idleError ?? MODEL_IDLE.note} tone={idleError ? "bad" : "secondary"}>
      <Segmented
        label={MODEL_IDLE.title}
        value={modelIdleValue(config.current?.modelIdleMinutes)}
        options={modelIdleOptions()}
        disabled={config.current === null || idleBusy}
        onchange={chooseModelIdle}
      />
    </Row>
  </Section>
</Form>
