<script lang="ts">
  import { status } from "../../lib/status.svelte";
  import { failure, recheck } from "../api";
  import { connectionRows, ocrConnect } from "../checklist";
  import { config } from "../config.svelte";
  import Row from "../kit/Row.svelte";
  import ConnectOcr from "../parts/ConnectOcr.svelte";

  // Step 3 of the setup: the answers of the checks, Check again, and for a server on another
  // machine the password form that connects the OCR, or connects it again when the server
  // refused its token. `connectError` is the error of the connection that followed the login
  // of step 2.
  let { hasLogin, connectError }: { hasLogin: boolean; connectError: string | null } = $props();

  let checkNote = $state<string | null>(null);

  const current = $derived(config.current);
  const rows = $derived(
    status.current && current ? connectionRows(status.current, current, hasLogin) : [],
  );
  const connectRow = $derived(ocrConnect(status.current, current));

  async function again() {
    checkNote = null;
    try {
      await recheck();
      checkNote = "Checked. The answers above are new.";
    } catch (error) {
      checkNote = failure(error);
    }
  }
</script>

{#each rows as row (row.title)}
  <Row title={row.title}><span class="value" data-tone={row.tone}>{row.value}</span></Row>
{/each}
<Row title="Check again" note={checkNote ?? "Notedown checks the server and the OCR now."}>
  <button type="button" class="button" onclick={again}>Check again</button>
</Row>
<ConnectOcr row={connectRow} {connectError} />

<style>
  .value[data-tone="good"] {
    color: var(--good);
  }

  .value[data-tone="bad"] {
    color: var(--bad);
  }
</style>
