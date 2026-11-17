<script lang="ts">
  import { onMount } from "svelte";
  import {
    failure,
    notificationAuthorization,
    openLink,
    setNotifications,
    type Authorization,
    type NotificationChoices,
  } from "../api";
  import { config } from "../config.svelte";
  import Form from "../kit/Form.svelte";
  import Row from "../kit/Row.svelte";
  import Section from "../kit/Section.svelte";
  import Switch from "../kit/Switch.svelte";
  import { DOT_LINE, NOTIFICATIONS_PANE, notificationRows, permissionReads, withChoice } from "../notifications";

  // Settings > Notifications: the switch, the choice per event and what macOS allows.
  let authorization = $state<Authorization | null>(null);
  let problem = $state<string | null>(null);
  // A change waits for the save and, when the switch goes on, for the answer of macOS.
  let busy = $state(false);
  // A permission read started before a change (or during one) is old.
  const reads = permissionReads(() => busy);

  const view = $derived(notificationRows(config.current, authorization));

  function readPermission() {
    reads.read(
      notificationAuthorization,
      (answer) => (authorization = answer),
      (error) => (problem = failure(error)),
    );
  }

  // Again on focus: the permission may have changed in System Settings.
  onMount(readPermission);

  async function change(key: keyof NotificationChoices, on: boolean) {
    const choices = config.current?.notifications;
    if (!choices || busy) return;
    busy = true;
    reads.changed();
    try {
      const answer = await setNotifications(withChoice(choices, key, on));
      config.current = answer.config;
      authorization = answer.authorization;
      problem = null;
    } catch (error) {
      problem = failure(error);
    } finally {
      busy = false;
    }
  }

  async function openSettings() {
    try {
      await openLink(NOTIFICATIONS_PANE);
    } catch (error) {
      problem = failure(error);
    }
  }
</script>

<svelte:window onfocus={readPermission} />

<Form>
  <Section footer={DOT_LINE}>
    <Row title={view.master.title} note={view.permission?.text} tone={view.permission?.tone ?? "secondary"}>
      {#if view.permission?.openSettings}
        <button type="button" class="button" onclick={openSettings}>Open System Settings</button>
      {/if}
      <Switch
        checked={view.master.checked}
        disabled={view.master.disabled || busy}
        label={view.master.title}
        onchange={(on) => change("enabled", on)}
      />
    </Row>
    {#if problem}
      <Row title="Not done" note={problem} tone="bad" />
    {/if}
  </Section>
  <Section>
    {#each view.events as row (row.key)}
      <Row title={row.title} note={row.note}>
        <Switch
          checked={row.checked}
          disabled={row.disabled || busy}
          label={row.title}
          onchange={(on) => change(row.key, on)}
        />
      </Row>
    {/each}
  </Section>
</Form>
