<script lang="ts">
  import { status } from "../../lib/status.svelte";
  import { failure, openRecycleBin, setSetupRecord, type SyncCheck, syncState } from "../api";
  import { config } from "../config.svelte";
  import { firstSyncView } from "../importFlow";
  import Banner from "../kit/Banner.svelte";
  import Row from "../kit/Row.svelte";

  // Step 6 of the setup, and the device steps on the Supernote page: how the Supernote logs in
  // to the server and syncs once, and what the check of that first sync found. With `check`
  // off (the Supernote page) only the device steps show.
  let { done = false, check = true }: { done?: boolean; check?: boolean } = $props();

  let checked = $state<SyncCheck | null>(null);
  let acknowledged = $state(false);
  let problem = $state<string | null>(null);

  const mode = $derived(config.current?.mode ?? "notSet");
  const urls = $derived(status.current?.server.deviceUrls ?? []);
  const view = $derived(firstSyncView(checked, acknowledged, mode));

  // The check is saved by Rust: read it again when the config or the last sync changes.
  $effect(() => {
    if (!check) return;
    void config.current;
    void status.current?.device.lastSyncMs;
    syncState()
      .then((state) => {
        checked = state.checked;
        acknowledged = state.acknowledged;
      })
      .catch(() => {});
  });

  async function seen() {
    problem = null;
    try {
      config.current = await setSetupRecord({ syncAcknowledged: true });
      acknowledged = true;
    } catch (error) {
      problem = failure(error);
    }
  }

  function bin() {
    problem = null;
    openRecycleBin().catch((error: unknown) => (problem = failure(error)));
  }
</script>

{#if check && view.banner}
  {#each view.banner as line, i (i)}
    <Banner message={line} />
  {/each}
  <Row title="Recycle bin" note="Restore what the Supernote should not have deleted.">
    <button type="button" class="button" onclick={bin}>Open the recycle bin</button>
    <button type="button" class="button strong" onclick={() => void seen()}>I have seen this</button>
  </Row>
{:else if check && view.line}
  <Row title="First sync" note={view.line} tone="good" />
{/if}
{#if check && !done}
  <Banner message="Do not write on the Supernote until this step is done." />
{/if}
<Row title="Ratta's cloud" note={'On the Supernote, log out of Ratta\'s cloud and choose "Retain locally".'} />
<Row
  title="Private Cloud"
  note={mode === "local" && urls.length > 0
    ? `Open Private Cloud and enter the address from step 1: ${urls[0]}`
    : "Open Private Cloud and enter the address of the server from step 1."}
/>
<Row title="Log in" note="Log in with the e-mail and the password from step 2." />
<Row title="Folders" note="Select the folders Note, Document, MyStyle, EXPORT, SCREENSHOT and INBOX." />
<!-- The Supernote page (check off) leaves the first-sync rows out: they hold only in setup. -->
{#if check && done}
  <Row title="Auto Sync" note="Turn Auto Sync on, on the Supernote." />
{:else if check}
  <Row title="Auto Sync" note="Leave Auto Sync off for now." />
  <Row title="Sync" note="Sync once by hand." />
{/if}
{#if problem}<Row title="Problem" note={problem} tone="bad" />{/if}
