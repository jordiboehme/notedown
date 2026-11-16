<script lang="ts">
  import { onMount } from "svelte";
  import type { ChildState, Mode } from "../../lib/api";
  import { status } from "../../lib/status.svelte";
  import { failure, processLogs, setMode, startProcesses, stopProcesses } from "../api";
  import { config } from "../config.svelte";
  import Banner from "../kit/Banner.svelte";
  import Form from "../kit/Form.svelte";
  import Row from "../kit/Row.svelte";
  import Section from "../kit/Section.svelte";
  import Segmented from "../kit/Segmented.svelte";
  import StatusDot from "../kit/StatusDot.svelte";

  // Settings > Server: where the server runs, the state of the server and the OCR pipeline,
  // Start and Stop, the address for the Supernote and the last lines of their output.
  type Where = "local" | "remote";

  let where = $state<Where>("local");
  let folder = $state("");
  let address = $state("");
  let problem = $state<string | null>(null);
  let saved = $state(false);
  let busy = $state(false);
  let logs = $state<{ server: string[]; pipeline: string[] }>({ server: [], pipeline: [] });

  const current = $derived(config.current);
  const changed = $derived(
    current !== null &&
      (current.mode !== where ||
        current.dataDir !== folder.trim() ||
        (where === "remote" && (current.serverUrl ?? "") !== address.trim())),
  );
  const server = $derived(status.current?.server ?? null);
  const pipeline = $derived(status.current?.pipeline ?? null);

  // The fields follow the config until the user edits them.
  let filled = false;
  $effect(() => {
    if (current && !filled) {
      filled = true;
      where = current.mode === "remote" ? "remote" : "local";
      folder = current.dataDir;
      address = current.serverUrl ?? "";
    }
  });

  async function readLogs() {
    try {
      logs = await processLogs();
    } catch {
      // The log is a help only; the state lines above say what matters.
    }
  }

  onMount(() => {
    void readLogs();
    const timer = setInterval(readLogs, 5_000);
    return () => clearInterval(timer);
  });

  async function act(run: () => Promise<unknown>) {
    busy = true;
    problem = null;
    saved = false;
    try {
      await run();
    } catch (error) {
      problem = failure(error);
    } finally {
      busy = false;
      void readLogs();
    }
  }

  const save = () =>
    act(async () => {
      const mode: Mode = where;
      const answer = await setMode(mode, folder, where === "remote" ? address : null);
      config.current = answer.config;
      folder = answer.config.dataDir;
      saved = true;
      problem = answer.keychain;
    });

  function level(state: ChildState | null): "good" | "warning" | "bad" | "unknown" {
    switch (state?.state) {
      case "running":
        return "good";
      case "starting":
      case "stopping":
      case "outside":
        return "warning";
      case "backoff":
        return "bad";
      default:
        return "unknown";
    }
  }

  const running = $derived(
    [server?.state, pipeline?.state].some((s) => s && ["running", "starting", "backoff"].includes(s.state)),
  );
</script>

<Form>
  <Section title="Where the server runs">
    <Row title="Server" note="This Mac runs the server for the Supernote, or another machine does.">
      <Segmented
        label="Where the server runs"
        value={where}
        options={[
          { value: "local", label: "This Mac" },
          { value: "remote", label: "Another machine" },
        ]}
        onchange={(next) => (where = next)}
      />
    </Row>
    {#if where === "remote"}
      <Row title="Address" note="The address of the Notedown server, such as http://pi.local:19072.">
        <input type="text" bind:value={address} placeholder="http://pi.local:19072" spellcheck="false" />
      </Row>
    {/if}
    <Row
      title="Data folder"
      note={where === "local" ? "Where the notes live on this Mac." : "Where the OCR keeps its work on this Mac."}
    >
      <input type="text" bind:value={folder} spellcheck="false" />
    </Row>
    <Row
      title="Save"
      note={problem ?? (saved ? "Saved. Notedown started again with it." : "Notedown starts again with the new place.")}
      tone={problem ? "bad" : saved ? "good" : "secondary"}
    >
      <button type="button" class="button strong" disabled={!changed || busy} onclick={save}>Save</button>
    </Row>
  </Section>

  {#if current && current.mode !== "notSet"}
    <Section title="State">
      {#if server && current.mode === "local"}
        <Row title="Server"><StatusDot level={level(server.state)} text={server.line} /></Row>
      {:else if server}
        <Row title="Server" note={server.line} />
      {/if}
      {#if pipeline}
        <Row title="OCR"><StatusDot level={level(pipeline.state)} text={pipeline.line} /></Row>
      {/if}
      <Row title="Start or stop" note="Stop ends the server and the OCR on this Mac. Start runs them again.">
        <button type="button" class="button" disabled={busy || !running} onclick={() => act(stopProcesses)}>Stop</button>
        <button type="button" class="button" disabled={busy} onclick={() => act(startProcesses)}>Start</button>
      </Row>
    </Section>
  {/if}

  {#if current?.mode === "local"}
    <Section title="Address for the Supernote">
      {#each server?.deviceUrls ?? [] as url (url)}
        <Row title={url} note="Enter this address in the private cloud settings of the Supernote." />
      {:else}
        <Row title="No address yet" note="The address shows while the server runs." />
      {/each}
      <Banner
        message="Give this Mac a fixed address in your router. The Supernote finds the server only at this address."
      />
    </Section>
  {/if}

  {#if logs.server.length > 0 || logs.pipeline.length > 0}
    <Section title="Last output">
      {#if logs.server.length > 0}
        <Row title="Server" />
        <pre class="log">{logs.server.join("\n")}</pre>
      {/if}
      {#if logs.pipeline.length > 0}
        <Row title="OCR" />
        <pre class="log">{logs.pipeline.join("\n")}</pre>
      {/if}
    </Section>
  {/if}
</Form>
