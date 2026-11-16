<script lang="ts">
  import { failure, logOut } from "../api";
  import { config } from "../config.svelte";
  import Form from "../kit/Form.svelte";
  import Row from "../kit/Row.svelte";
  import Section from "../kit/Section.svelte";

  // Settings > Account: who is logged in where. Log out deletes the login of this Mac.
  let note = $state<{ text: string; tone: "secondary" | "good" | "bad" } | null>(null);
  let busy = $state(false);

  const current = $derived(config.current);
  const server = $derived(
    current?.mode === "local" ? "This Mac" : current?.mode === "remote" ? (current.serverUrl ?? "Not set") : "Not set up yet",
  );

  async function leave() {
    busy = true;
    try {
      await logOut();
      note = { text: "Logged out. The login is deleted from this Mac.", tone: "good" };
    } catch (error) {
      note = { text: failure(error), tone: "bad" };
    } finally {
      busy = false;
    }
  }
</script>

<Form>
  <Section title="Account">
    <Row title="Account"><span class="value">{current?.account ?? "None yet"}</span></Row>
    <Row title="E-mail"><span class="value">{current?.email ?? "None yet"}</span></Row>
    <Row title="Server"><span class="value">{server}</span></Row>
    <Row title="Write access" note="The app may change files on the Supernote only with write access.">
      <span class="value">Not known yet</span>
    </Row>
  </Section>
  <Section>
    <Row
      title="Log out"
      note={note?.text ?? "Deletes the login from this Mac. The notes stay where they are."}
      tone={note?.tone ?? "secondary"}
    >
      <button type="button" class="button" disabled={busy || !current?.account} onclick={leave}>Log out</button>
    </Row>
  </Section>
</Form>
