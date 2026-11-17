<script lang="ts">
  import { relativeTime } from "../../lib/format";
  import { status } from "../../lib/status.svelte";
  import Form from "../kit/Form.svelte";
  import Row from "../kit/Row.svelte";
  import Section from "../kit/Section.svelte";
  import McpConfig from "../parts/McpConfig.svelte";

  // Settings > Agents: what each agent has read, and the MCP config for a new one.
  let { now }: { now: number } = $props();

  const agents = $derived(status.current?.agents ?? []);

  function when(ms: number | null, what: string, never: string): string {
    return ms === null ? never : `${what} ${relativeTime(ms, now)}.`;
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
    <McpConfig />
  </Section>
</Form>
