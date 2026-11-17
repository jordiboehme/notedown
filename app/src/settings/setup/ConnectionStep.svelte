<script lang="ts">
  import { status } from "../../lib/status.svelte";
  import { connectPipeline, failure, recheck } from "../api";
  import { connectionRows } from "../checklist";
  import { config } from "../config.svelte";
  import Row from "../kit/Row.svelte";

  // Step 3 of the setup: the answers of the checks, Check again, and for a server on another
  // machine without an OCR connection the password form that connects it. `connectError` is
  // the error of the connection that followed the login of step 2.
  let { hasLogin, connectError }: { hasLogin: boolean; connectError: string | null } = $props();

  let password = $state("");
  let busy = $state(false);
  let note = $state<{ text: string; tone: "secondary" | "good" | "bad" } | null>(null);
  let checkNote = $state<string | null>(null);

  const current = $derived(config.current);
  const rows = $derived(
    status.current && current ? connectionRows(status.current, current, hasLogin) : [],
  );
  const needsPipeline = $derived(current?.mode === "remote" && status.current?.existing.pipeline === false);

  async function again() {
    checkNote = null;
    try {
      await recheck();
      checkNote = "Checked. The answers above are new.";
    } catch (error) {
      checkNote = failure(error);
    }
  }

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

{#each rows as row (row.title)}
  <Row title={row.title}><span class="value" data-tone={row.tone}>{row.value}</span></Row>
{/each}
<Row title="Check again" note={checkNote ?? "Notedown checks the server and the OCR now."}>
  <button type="button" class="button" onclick={again}>Check again</button>
</Row>
{#if needsPipeline}
  <form onsubmit={connect}>
    <Row
      title="Connect the OCR"
      note={note?.text ?? connectError ?? "The OCR on this Mac logs in to the server with your password."}
      tone={note?.tone ?? (connectError ? "bad" : "secondary")}
    >
      <input type="password" bind:value={password} autocomplete="current-password" aria-label="Password" />
      <button type="submit" class="button strong" disabled={busy || password === ""}>Connect</button>
    </Row>
  </form>
{:else if note}
  <Row title="Connect the OCR" note={note.text} tone={note.tone} />
{/if}

<style>
  form {
    display: contents;
  }

  .value[data-tone="good"] {
    color: var(--good);
  }

  .value[data-tone="bad"] {
    color: var(--bad);
  }
</style>
