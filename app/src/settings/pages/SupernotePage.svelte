<script lang="ts">
  import { relativeTime } from "../../lib/format";
  import { status } from "../../lib/status.svelte";
  import Form from "../kit/Form.svelte";
  import Row from "../kit/Row.svelte";
  import Section from "../kit/Section.svelte";
  import StatusDot from "../kit/StatusDot.svelte";

  // Settings > Supernote: the last sync, a sync that runs now, and whether it is online.
  let { now }: { now: number } = $props();

  const device = $derived(status.current?.device ?? null);
  const urls = $derived(status.current?.server.deviceUrls ?? []);
</script>

<Form>
  <Section title="Supernote">
    {#if device === null}
      <Row title="Status" note="Looking at the status..." />
    {:else if device.line !== null}
      <Row title="Status" note={device.line} />
    {:else}
      <Row title="Device">
        <span class="value">{device.name ?? "Supernote"}</span>
      </Row>
      <Row title="Last sync" note="The last sync that ended well.">
        <span class="value">{device.lastSyncMs === null ? "Not synced yet" : relativeTime(device.lastSyncMs, now)}</span>
      </Row>
      <Row title="Syncing now">
        <StatusDot level={device.syncing ? "warning" : "unknown"} text={device.syncing ? "Syncing" : "No"} />
      </Row>
      <Row title="Online" note="The Supernote keeps a connection for automatic sync while it is on.">
        <StatusDot level={device.online ? "good" : "unknown"} text={device.online ? "Online" : "Offline"} />
      </Row>
    {/if}
  </Section>
  {#if urls.length > 0}
    <Section title="Address for the Supernote" footer="Enter this address in the private cloud settings of the Supernote.">
      {#each urls as url (url)}
        <Row title={url} />
      {/each}
    </Section>
  {/if}
</Form>
