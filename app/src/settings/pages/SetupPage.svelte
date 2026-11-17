<script lang="ts">
  import { onMount } from "svelte";
  import { status } from "../../lib/status.svelte";
  import {
    type ConfigAnswer,
    type ExistingFolder,
    existingDetails,
    hasLogin,
    homeDir,
    type ModelState,
    modelState,
  } from "../api";
  import type { LogInOutcome } from "../account";
  import { checklist, type ChecklistInput, type Step, type StepId, startOffered, stepDot } from "../checklist";
  import { config } from "../config.svelte";
  import Form from "../kit/Form.svelte";
  import Row from "../kit/Row.svelte";
  import Section from "../kit/Section.svelte";
  import StatusDot from "../kit/StatusDot.svelte";
  import McpConfig from "../parts/McpConfig.svelte";
  import ModelDownload from "../parts/ModelDownload.svelte";
  import AccountStep from "../setup/AccountStep.svelte";
  import ConnectionStep from "../setup/ConnectionStep.svelte";
  import ImportStep from "../setup/ImportStep.svelte";
  import { flow, startImportFlow } from "../setup/importState.svelte";
  import ServerStep from "../setup/ServerStep.svelte";
  import SupernoteStep from "../setup/SupernoteStep.svelte";

  // Settings > Setup: the seven steps from a new Mac to a working setup. Each step shows its
  // state from the checklist (checklist.ts) and its controls; a step that waits for an
  // earlier one shows only which one.
  let { onOpen }: { onOpen: (url: string) => void } = $props();

  let details = $state<ExistingFolder[]>([]);
  let home = $state<string | null>(null);
  let model = $state<ModelState | null>(null);
  let loggedIn = $state(false);
  const startEmail = config.current?.email ?? "";
  let email = $state(startEmail);
  let connectError = $state<string | null>(null);

  const input = $derived<ChecklistInput | null>(
    status.current && config.current
      ? {
          status: status.current,
          config: config.current,
          model,
          existing: status.current.existing,
          details,
          job: flow.job,
          hasLogin: loggedIn,
          dryRun: flow.dryRun && { filesPresent: flow.dryRun.filesPresent },
        }
      : null,
  );
  const steps = $derived(input ? checklist(input) : null);
  const step = (id: StepId): Step | undefined => steps?.find((s) => s.id === id);
  const number = (id: StepId) => (steps ? steps.findIndex((s) => s.id === id) + 1 : 0);
  const open = (id: StepId) => {
    const state = step(id)?.state;
    return state !== undefined && state !== "blocked";
  };

  function readDetails() {
    existingDetails()
      .then((found) => (details = found))
      .catch(() => (details = []));
  }

  // The login belongs to the account: read it again when the place or the account changes
  // (also when the Account page logs out).
  $effect(() => {
    void config.current?.mode;
    void config.current?.serverUrl;
    void config.current?.account;
    hasLogin()
      .then((kept) => (loggedIn = kept))
      .catch(() => (loggedIn = false));
  });

  // The e-mail starts as the config's, once; the user may empty it.
  let emailFilled = startEmail !== "";
  $effect(() => {
    const kept = config.current?.email;
    if (kept && !emailFilled) {
      emailFilled = true;
      email = kept;
    }
  });

  onMount(() => {
    readDetails();
    homeDir()
      .then((found) => (home = found))
      .catch(() => (home = null));
    // The model: read once here, then each change from the download part of step 4.
    modelState()
      .then((m) => (model = m))
      .catch(() => {});
    // The steps' events are followed for the life of the window (importState.svelte.ts).
    startImportFlow();
  });

  function saved(answer: ConfigAnswer, found?: string) {
    if (found) email = found;
    else if (answer.config.email) email = answer.config.email;
    connectError = null;
    readDetails();
  }

  function loggedInNow(outcome: LogInOutcome) {
    loggedIn = true;
    connectError = outcome.connectError;
  }
</script>

{#snippet head(id: StepId)}
  {@const s = step(id)}
  {#if s}
    {@const dot = stepDot(s.state)}
    <Row title="State" note={s.line} tone={s.state === "done" ? "good" : "secondary"}>
      <StatusDot level={dot.level} text={dot.text} />
    </Row>
  {/if}
{/snippet}

<Form>
  {#if steps === null}
    <Section title="Setup">
      <Row title="Setup" note="Looking at the setup..." />
    </Section>
  {:else}
    <Section title={`${number("server")}. ${step("server")?.title}`}>
      {@render head("server")}
      <ServerStep {details} {home} start={input !== null && startOffered(input)} onSaved={saved} {onOpen} />
    </Section>

    <Section title={`${number("account")}. ${step("account")?.title}`}>
      {@render head("account")}
      {#if open("account")}
        <AccountStep
          bind:email
          existing={status.current?.existing ?? { server: false, pipeline: false }}
          onLoggedIn={loggedInNow}
          {onOpen}
        />
      {/if}
    </Section>

    <Section title={`${number("connection")}. ${step("connection")?.title}`}>
      {@render head("connection")}
      {#if open("connection")}
        <ConnectionStep hasLogin={loggedIn} {connectError} />
      {/if}
    </Section>

    <Section title={`${number("model")}. ${step("model")?.title}`}>
      {@render head("model")}
      <ModelDownload onchange={(m) => (model = m)} />
    </Section>

    <Section title={`${number("import")}. ${step("import")?.title}`}>
      {@render head("import")}
      {@const imports = step("import")}
      {#if imports}<ImportStep step={imports} {onOpen} />{/if}
    </Section>

    <Section title={`${number("supernote")}. ${step("supernote")?.title}`}>
      {@render head("supernote")}
      {#if open("supernote")}
        <SupernoteStep done={step("supernote")?.state === "done"} />
      {/if}
    </Section>

    <Section title={`${number("agent")}. ${step("agent")?.title}`}>
      {@render head("agent")}
      <McpConfig />
    </Section>
  {/if}
</Form>
