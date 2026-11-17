<script lang="ts">
  import { open } from "@tauri-apps/plugin-dialog";
  import { onMount } from "svelte";
  import { status } from "../../lib/status.svelte";
  import { defaultBackupDir, failure, SETUP_LINKS } from "../api";
  import type { Step } from "../checklist";
  import { config } from "../config.svelte";
  import {
    appDataLines,
    backupProgressText,
    backupResumable,
    endedText,
    type FlowInput,
    importAgainOffered,
    importFailure,
    importPanel,
    importProgressText,
    importRefusal,
    importStage,
    reviewActions,
    reviewLines,
    snbakRefusal,
    tooOldLine,
  } from "../importFlow";
  import Banner from "../kit/Banner.svelte";
  import Row from "../kit/Row.svelte";
  import {
    check,
    chooseAnotherSnbak,
    chooseSnbak,
    flow,
    importAppData,
    importFiles,
    makeBackup,
    newBackup,
    noDigests,
    pauseBackup,
    readChoices,
    resumeBackup,
    startAgain,
  } from "./importState.svelte";

  // Step 5 of the setup: the backup of the Supernote, the review and the import of its files,
  // then the .snbak and the import of its digests (importFlow.ts says where it stands). It
  // stays shown while the step is blocked after an import (the server starts again).
  let { step, onOpen }: { step: Step; onOpen: (url: string) => void } = $props();

  let now = $state(Date.now());
  let url = $state(config.current?.setup.backupUrl ?? "");
  let dir = $state(config.current?.setup.backupDir ?? defaultBackupDir(new Date()));

  const mode = $derived(config.current?.mode ?? "notSet");
  const account = $derived(status.current?.account ?? null);
  const lastSyncMs = $derived(status.current?.device.lastSyncMs ?? null);
  const serverState = $derived(status.current?.server.state?.state ?? null);
  const backupTimeMs = $derived(
    flow.backup && (flow.backup.state.state === "complete" || flow.backup.state.state === "resumable")
      ? Date.parse(flow.backup.state.time)
      : Number.NaN,
  );
  const input = $derived<FlowInput>({
    mode,
    account,
    job: flow.job,
    nowMs: now,
    lastSyncMs,
    backup: flow.backup && { state: flow.backup.state, madeThisRun: flow.backup.madeThisRun },
    dryRun: flow.dryRun,
    snbak: flow.snbak && { timeMs: flow.snbak.timeMs, chosenThisRun: flow.snbak.chosenThisRun },
    appData: flow.appData,
    last: flow.last,
    again: flow.again,
    filesThisRun: flow.filesThisRun,
    appDataThisRun: flow.appDataThisRun,
  });
  const stage = $derived(importStage(input));
  const refusal = $derived(importRefusal(input));
  const panel = $derived(importPanel({ state: step.state, job: flow.job, last: flow.last, mode, server: serverState }));
  const actions = $derived(flow.dryRun ? reviewActions(flow.dryRun) : null);
  const lastText = $derived(flow.last && !flow.last.ok ? endedText(flow.last) : null);
  const snbakProblem = $derived(
    flow.snbak && flow.snbak.path !== null ? snbakRefusal(flow.snbak.timeMs, now) : null,
  );

  // Why the .snbak cannot be used: too old, or its check failed.
  const snbakNote = $derived(snbakProblem ?? (flow.last?.kind === "appDataDryRun" ? lastText : null));

  onMount(() => {
    void readChoices();
    const clock = setInterval(() => (now = Date.now()), 30_000);
    return () => clearInterval(clock);
  });

  async function chooseFolder() {
    const picked = await open({ directory: true, multiple: false, title: "Choose an empty folder for the backup" }).catch(
      (error: unknown) => {
        flow.error = failure(error);
        return null;
      },
    );
    if (typeof picked === "string") dir = picked;
  }

  async function pickSnbak() {
    const picked = await open({
      directory: false,
      multiple: false,
      title: "Choose the .snbak file",
      filters: [{ name: "Supernote backup", extensions: ["snbak"] }],
    }).catch((error: unknown) => {
      flow.error = failure(error);
      return null;
    });
    if (typeof picked === "string") await chooseSnbak(picked);
  }

  async function freshBackup() {
    dir = await newBackup(new Date());
  }

  async function again() {
    startAgain();
    dir = await newBackup(new Date());
  }

  function backupNow(event: SubmitEvent) {
    event.preventDefault();
    if (url.trim() === "" || dir.trim() === "") return;
    now = Date.now();
    void makeBackup(url.trim(), dir.trim());
  }
</script>

{#if mode === "remote"}
  {#if step.state !== "blocked"}
    <Row title="Import" note="Import the Supernote on the other machine.">
      <button type="button" class="button" onclick={() => onOpen(SETUP_LINKS.otherMachine)}>Instructions</button>
    </Row>
  {/if}
{:else if panel.show}
  {#if stage !== "done"}
    <Banner message="Do not write on the Supernote until the first sync in step 6 is done." />
  {/if}
  {#if panel.line}<Row title="Server" note={panel.line} tone="bad" />{/if}
  {#if flow.again && stage !== "done"}
    <Banner
      message="Import again only if the server lost files. Make a new backup first: a file that is missing on the server is deleted on the Supernote."
    />
  {/if}

  {#if stage === "address"}
    {#if flow.again}
      <Banner message="Turn Auto Sync off on the Supernote before you make the new backup." />
    {/if}
    <form onsubmit={backupNow}>
      <Row
        title="Supernote address"
        note="On the Supernote, open Settings, then Browse & Access, and turn it on. Enter the address it shows."
      >
        <input type="text" bind:value={url} placeholder="http://192.168.1.20:8089" aria-label="Supernote address" />
      </Row>
      <Row title="Backup folder" note={dir} path>
        <button type="button" class="button" onclick={chooseFolder}>Choose...</button>
      </Row>
      <Row
        title="Backup"
        note={(flow.last?.kind === "backup" && lastText) || "Notedown copies every file of the Supernote into the folder."}
        tone={flow.last?.kind === "backup" && lastText ? "bad" : "secondary"}
      >
        <button type="submit" class="button strong" disabled={url.trim() === "" || dir.trim() === ""}>
          Make a backup
        </button>
      </Row>
      {#if flow.backup !== null}
        <Row title="New folder" note="Make the backup into a new, empty folder.">
          <button type="button" class="button" onclick={freshBackup}>Make a new backup</button>
        </Row>
      {/if}
    </form>
  {:else if stage === "backingUp"}
    <Row
      title="Backing up"
      note={flow.progress?.files != null ? backupProgressText(flow.progress.files, flow.progress.bytes ?? 0) : "Starting the backup."}
    >
      <button type="button" class="button" onclick={() => void pauseBackup()}>Pause</button>
    </Row>
  {:else if stage === "paused"}
    <Row title="Backup" note="The backup did not finish. Resume continues it in the same folder.">
      <button type="button" class="button strong" onclick={() => void resumeBackup()}>Resume</button>
      <button type="button" class="button" onclick={freshBackup}>Make a new backup</button>
    </Row>
  {:else if stage === "backupDone"}
    <Row title="Backup" note={lastText ?? "The backup ended with errors."} tone="bad" />
    {#each Array.isArray(flow.last?.json?.errors) ? (flow.last?.json?.errors as { path: string; message: string }[]) : [] as error, i (i)}
      <Row title={error.path} note={error.message} tone="bad" />
    {/each}
    {#if flow.backup && backupResumable(flow.backup.state)}
      <Row title="Try again" note="Resume fetches the files that failed.">
        <button type="button" class="button strong" onclick={() => void resumeBackup()}>Resume</button>
        <button type="button" class="button" onclick={freshBackup}>Make a new backup</button>
      </Row>
    {:else}
      <Row title="Try again" note="Make a new backup into a new folder.">
        <button type="button" class="button strong" onclick={freshBackup}>Make a new backup</button>
      </Row>
    {/if}
  {:else if stage === "tooOld"}
    <Row title="Backup" note={tooOldLine(backupTimeMs, now, lastSyncMs)} tone="bad">
      <button type="button" class="button strong" onclick={freshBackup}>Make a new backup</button>
    </Row>
  {:else if stage === "review"}
    {#if flow.job === "importDryRun"}
      <Row title="Review" note="Checking the backup." />
    {:else if flow.dryRun && actions}
      {#each reviewLines(flow.dryRun, backupTimeMs, now, flow.again) as line, i (i)}
        <Row title={i === 0 ? "Review" : ""} note={line} tone={line.startsWith("Error:") ? "bad" : "secondary"} />
      {/each}
      {#if actions.replace && !flow.again}
        <Row title="Replace" note="Move the files on the server to the recycle bin first.">
          <input type="checkbox" bind:checked={flow.replace} aria-label="Replace" />
        </Row>
      {/if}
      <Row
        title="Import"
        note={refusal ?? "Until the import is done, the Supernote cannot sync with this server."}
        tone={refusal ? "bad" : "secondary"}
      >
        <button
          type="button"
          class="button strong"
          disabled={refusal !== null || !actions.canImport || (actions.replace && !flow.replace && !flow.again)}
          onclick={() => void importFiles(actions.empty)}>{actions.empty ? "My Supernote is empty" : "Import"}</button
        >
      </Row>
      {#if !actions.canImport}
        <Row title="New backup" note="The import refuses this backup. Make a new one into a new folder.">
          <button type="button" class="button" onclick={freshBackup}>Make a new backup</button>
        </Row>
      {/if}
    {:else}
      <Row title="Review" note={lastText ?? "Check the backup before the import."} tone={lastText ? "bad" : "secondary"}>
        <button type="button" class="button strong" onclick={() => void check()}>Check the backup</button>
        <button type="button" class="button" onclick={freshBackup}>Make a new backup</button>
      </Row>
    {/if}
  {:else if stage === "importing"}
    <Row
      title="Importing"
      note={(flow.progress && importProgressText(flow.progress)) ?? "Importing the files. The server is stopped meanwhile."}
    />
  {:else if stage === "failed" && flow.last}
    {@const failed = importFailure(flow.last)}
    <Row title="Import" note={refusal ?? failed.text} tone="bad">
      <button
        type="button"
        class="button strong"
        disabled={refusal !== null}
        onclick={() => void importFiles(flow.dryRun?.files === 0, failed.replace)}
      >
        Try again with Replace
      </button>
    </Row>
  {:else if stage === "snbak"}
    <Row
      title="Digests"
      note={snbakNote ?? "Make the Supernote's own backup (Settings, then Backup) and copy the .snbak file to this Mac."}
      tone={snbakNote ? "bad" : "secondary"}
    >
      <button type="button" class="button strong" onclick={pickSnbak}>Choose the .snbak file...</button>
      <button type="button" class="button" onclick={() => void noDigests()}>My Supernote has no digests</button>
    </Row>
    {#if flow.last?.kind === "appDataDryRun" && !flow.again}
      <Row title="Replace" note="Clear the digests on the server first.">
        <input type="checkbox" bind:checked={flow.appDataReplace} aria-label="Replace the digests" />
      </Row>
    {/if}
  {:else if stage === "appDataReview"}
    {#if flow.job === "appDataDryRun" || !flow.appData}
      <Row title="Digests" note="Checking the .snbak file." />
    {:else}
      {#each appDataLines(flow.appData) as line, i (i)}
        <Row title={i === 0 ? "Digests" : ""} note={line} />
      {/each}
      <Row title="Import" note="Until the import is done, the Supernote cannot sync with this server.">
        <button type="button" class="button strong" onclick={() => void importAppData()}>Import the digests</button>
        <button type="button" class="button" onclick={chooseAnotherSnbak}>Choose another file</button>
      </Row>
    {/if}
  {:else if stage === "appDataImporting"}
    <Row
      title="Importing"
      note={(flow.progress && importProgressText(flow.progress)) ?? "Importing the digests. The server is stopped meanwhile."}
    />
  {:else if stage === "appDataFailed"}
    <Row title="Digests" note={`The import of the digests did not finish: ${lastText ?? "it stopped"}`} tone="bad">
      <button type="button" class="button strong" onclick={() => void importAppData(true)}>Try again with Replace</button>
      <button type="button" class="button" onclick={chooseAnotherSnbak}>Choose another file</button>
    </Row>
  {:else if stage === "done"}
    <Row title="Import" note="The files and the digests of the Supernote are on the server." tone="good" />
    {#if importAgainOffered(mode, account, lastSyncMs)}
      <Row
        title="Import again"
        note="Import again only if the server lost files. Make a new backup first: a file that is missing on the server is deleted on the Supernote."
      >
        <button type="button" class="button" onclick={again}>Import again</button>
      </Row>
    {/if}
  {/if}
  {#if flow.error}<Row title="Problem" note={flow.error} tone="bad" />{/if}
{/if}

<style>
  form {
    display: contents;
  }
</style>
