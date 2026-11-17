<script lang="ts">
  import { status } from "../../lib/status.svelte";
  import { type AppConfig, type ConfigAnswer, type ExistingFolder, failure, setMode, SETUP_LINKS } from "../api";
  import { useFolder } from "../account";
  import { autostartLine, foundLine, startFolder } from "../checklist";
  import { config } from "../config.svelte";
  import Row from "../kit/Row.svelte";
  import Segmented from "../kit/Segmented.svelte";
  import DeviceAddress from "../parts/DeviceAddress.svelte";

  // Step 1 of the setup: where the server runs, the data folder, the address of a server on
  // another machine, the folders found on this Mac with Use it, and the address for the
  // Supernote. `onSaved` gets the e-mail of a found folder with one account.
  let {
    details,
    onSaved,
    onOpen,
  }: {
    details: ExistingFolder[];
    onSaved: (answer: ConfigAnswer, email?: string) => void;
    onOpen: (url: string) => void;
  } = $props();

  type Where = "local" | "remote";

  let where = $state<Where>("local");
  let folder = $state("");
  let address = $state("");
  let edited = false;
  let note = $state<{ text: string; tone: "secondary" | "good" | "bad" } | null>(null);
  let autostart = $state<string | null>(null);
  let busy = $state(false);

  const current = $derived(config.current);
  const changed = $derived(
    current !== null &&
      (current.mode !== where ||
        current.dataDir !== folder.trim() ||
        (where === "remote" && (current.serverUrl ?? "") !== address.trim())),
  );

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

  function choose(next: Where) {
    where = next;
    if (current && !edited) folder = startFolder(next, current, details);
  }

  async function save(mode: Where, dataDir: string, serverUrl: string | null, email?: string) {
    busy = true;
    note = null;
    const before: AppConfig | null = config.current;
    try {
      const answer = await setMode(mode, dataDir, serverUrl);
      config.current = answer.config;
      where = answer.config.mode === "remote" ? "remote" : "local";
      folder = answer.config.dataDir;
      address = answer.config.serverUrl ?? "";
      edited = false;
      autostart = autostartLine(before, answer.config) ?? autostart;
      note = answer.keychain
        ? { text: answer.keychain, tone: "bad" }
        : { text: "Saved. Notedown started again with it.", tone: "good" };
      onSaved(answer, email);
    } catch (error) {
      note = { text: failure(error), tone: "bad" };
    } finally {
      busy = false;
    }
  }

  function use(found: ExistingFolder) {
    const used = useFolder(found);
    void save(used.mode, used.dataDir, used.serverUrl, used.email);
  }
</script>

{#each details as found (found.path)}
  <Row title="Found on this Mac" note={foundLine(found)}>
    <button type="button" class="button" disabled={busy} onclick={() => use(found)}>Use it</button>
  </Row>
{/each}
<Row title="Server" note="This Mac runs the server for the Supernote, or another machine does.">
  <Segmented
    label="Where the server runs"
    value={where}
    options={[
      { value: "local", label: "This Mac" },
      { value: "remote", label: "Another machine" },
    ]}
    onchange={choose}
  />
</Row>
{#if where === "remote"}
  <Row title="Address" note="The address of the Notedown server, such as http://pi.local:19072.">
    <input type="text" bind:value={address} placeholder="http://pi.local:19072" spellcheck="false" />
  </Row>
  <Row title="Instructions" note="How to set up Notedown on the other machine.">
    <button type="button" class="button" onclick={() => onOpen(SETUP_LINKS.otherMachine)}>
      The server on another machine
    </button>
  </Row>
{/if}
<Row
  title="Data folder"
  note={where === "local" ? "Where the notes live on this Mac." : "Where the OCR keeps its work on this Mac."}
>
  <input type="text" bind:value={folder} oninput={() => (edited = true)} spellcheck="false" />
</Row>
<Row
  title="Save"
  note={note?.text ?? "Notedown starts again with the new place."}
  tone={note?.tone ?? "secondary"}
>
  <button
    type="button"
    class="button strong"
    disabled={!changed || busy}
    onclick={() => save(where, folder, where === "remote" ? address : null)}>Save</button
  >
</Row>
{#if autostart}<Row title="Start at login" note={autostart} tone="good" />{/if}
{#if current?.mode === "local"}
  <DeviceAddress urls={status.current?.server.deviceUrls ?? []} />
{/if}
