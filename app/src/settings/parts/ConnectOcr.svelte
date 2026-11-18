<script lang="ts">
  import { connectPipeline, failure, recheck } from "../api";
  import type { OcrConnect } from "../checklist";
  import { config } from "../config.svelte";
  import Row from "../kit/Row.svelte";

  // The Connect the OCR row of step 3 and the OCR page: a password field and Connect (or
  // Connect again when the server refused the OCR's token). `row` is null when nothing is to
  // do; `connectError` is the error of the connection that followed a login. The password
  // goes to the sidecar over stdin only.
  let { row, connectError = null }: { row: OcrConnect | null; connectError?: string | null } = $props();

  let password = $state("");
  let busy = $state(false);
  let note = $state<{ text: string; tone: "secondary" | "good" | "bad" } | null>(null);

  async function connect(event: SubmitEvent) {
    event.preventDefault();
    if (busy || password === "") return;
    busy = true;
    note = null;
    try {
      const answer = await connectPipeline(password);
      config.current = answer.config;
      note = { text: "The OCR on this Mac is connected to the server.", tone: "good" };
      void recheck().catch(() => {});
    } catch (error) {
      note = { text: failure(error), tone: "bad" };
    } finally {
      password = "";
      busy = false;
    }
  }
</script>

{#if row}
  <form onsubmit={connect}>
    <Row
      title="Connect the OCR"
      note={note?.text ?? connectError ?? row.note}
      tone={note?.tone ?? (connectError ? "bad" : "secondary")}
    >
      <input type="password" bind:value={password} autocomplete="current-password" aria-label="Password" />
      <button type="submit" class="button strong" disabled={busy || password === ""}>{row.button}</button>
    </Row>
  </form>
{:else if note}
  <Row title="Connect the OCR" note={note.text} tone={note.tone} />
{/if}

<style>
  form {
    display: contents;
  }
</style>
