<script lang="ts">
  import { onMount } from "svelte";
  import { failure, mcpInfo } from "../api";
  import Banner from "../kit/Banner.svelte";
  import Row from "../kit/Row.svelte";
  import { mcpConfig, pathWarning } from "../mcp";

  // The MCP config for a new agent, with Copy and the warning about the app's place. The
  // Agents page and step 7 of the setup show it.
  let text = $state<string | null>(null);
  let warning = $state<string | null>(null);
  let note = $state<{ text: string; tone: "secondary" | "good" | "bad" } | null>(null);

  onMount(() => {
    mcpInfo()
      .then((info) => {
        text = mcpConfig(info.sidecar, info.dataDir);
        warning = pathWarning(info.sidecar);
      })
      .catch((error: unknown) => (note = { text: failure(error), tone: "bad" }));
  });

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

<Row
  title="MCP config"
  note={note?.text ?? "Gives an agent the notes over MCP. Copy it into the MCP settings of the agent."}
  tone={note?.tone ?? "secondary"}
>
  <button type="button" class="button" disabled={text === null} onclick={copy}>Copy MCP config</button>
</Row>
{#if warning}<Banner message={warning} />{/if}
{#if text}<pre class="log">{text}</pre>{/if}
