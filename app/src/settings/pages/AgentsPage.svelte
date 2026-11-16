<script lang="ts">
  import { onMount } from "svelte";
  import { relativeTime } from "../../lib/format";
  import { status } from "../../lib/status.svelte";
  import { failure, mcpInfo } from "../api";
  import Banner from "../kit/Banner.svelte";
  import Form from "../kit/Form.svelte";
  import Row from "../kit/Row.svelte";
  import Section from "../kit/Section.svelte";
  import { mcpConfig, pathWarning } from "../mcp";

  // Settings > Agents: what each agent has read, and the MCP config for a new one.
  let { now }: { now: number } = $props();

  let text = $state<string | null>(null);
  let warning = $state<string | null>(null);
  let note = $state<{ text: string; tone: "secondary" | "good" | "bad" } | null>(null);

  const agents = $derived(status.current?.agents ?? []);

  onMount(() => {
    mcpInfo()
      .then((info) => {
        text = mcpConfig(info.sidecar, info.dataDir);
        warning = pathWarning(info.sidecar);
      })
      .catch((error: unknown) => (note = { text: failure(error), tone: "bad" }));
  });

  function when(ms: number | null, what: string, never: string): string {
    return ms === null ? never : `${what} ${relativeTime(ms, now)}.`;
  }

  async function copy() {
    if (text === null) return;
    try {
      await navigator.clipboard.writeText(text);
      note = { text: "Copied. Paste it into the MCP settings of the agent.", tone: "good" };
    } catch {
      note = { text: "Copying did not work. Select the config below and copy it.", tone: "bad" };
    }
  }
</script>

<Form>
  <Section title="Agents">
    {#if agents.length === 0}
      <Row title="No agent yet" note="An agent shows here once it reads the notes over MCP." />
    {:else}
      {#each agents as agent (agent.name)}
        <Row
          title={agent.name}
          note={`${when(agent.readAtMs, "Read", "Not read yet.")} ${when(agent.ackedAtMs, "Confirmed", "Not confirmed yet.")}`}
        >
          <span class="value">{agent.behind}</span>
        </Row>
      {/each}
    {/if}
  </Section>
  <Section title="Connect an agent">
    <Row
      title="MCP config"
      note={note?.text ?? "Gives an agent the notes over MCP. Copy it into the MCP settings of the agent."}
      tone={note?.tone ?? "secondary"}
    >
      <button type="button" class="button" disabled={text === null} onclick={copy}>Copy MCP config</button>
    </Row>
    {#if warning}<Banner message={warning} />{/if}
    {#if text}<pre class="log">{text}</pre>{/if}
  </Section>
</Form>
