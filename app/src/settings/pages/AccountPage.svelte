<script lang="ts">
  import { onMount } from "svelte";
  import { failure, hasLogin, logIn, logOut } from "../api";
  import { accountActions } from "../account";
  import { config } from "../config.svelte";
  import Form from "../kit/Form.svelte";
  import Row from "../kit/Row.svelte";
  import Section from "../kit/Section.svelte";

  // Settings > Account: who is logged in where. Log in keeps a write token in the Keychain;
  // Log out deletes the login of this Mac and forgets the account.
  type Note = { text: string; tone: "secondary" | "good" | "bad" };
  let loginNote = $state<Note | null>(null);
  let logoutNote = $state<Note | null>(null);
  let busy = $state<"logIn" | "logOut" | null>(null);
  let email = $state(config.current?.email ?? "");
  let password = $state("");
  let loggedIn = $state(false);

  onMount(() => {
    hasLogin()
      .then((kept) => (loggedIn = kept))
      .catch(() => (loggedIn = false));
  });

  const current = $derived(config.current);
  const server = $derived(
    current?.mode === "local" ? "This Mac" : current?.mode === "remote" ? (current.serverUrl ?? "Not set") : "Not set up yet",
  );
  const actions = $derived(
    accountActions({
      mode: current?.mode ?? null,
      account: current?.account ?? null,
      accountEmail: current?.email ?? null,
      loggedIn,
      email,
      password,
      busy,
    }),
  );

  async function enter(event: SubmitEvent) {
    event.preventDefault();
    if (!actions.logIn.enabled) return;
    busy = "logIn";
    loginNote = null;
    try {
      config.current = await logIn(email, password);
      loggedIn = true;
      logoutNote = null;
      loginNote = { text: `Logged in as ${config.current.account}.`, tone: "good" };
    } catch (error) {
      loginNote = { text: failure(error), tone: "bad" };
    } finally {
      password = "";
      busy = null;
    }
  }

  async function leave() {
    busy = "logOut";
    try {
      config.current = await logOut();
      loggedIn = false;
      loginNote = null;
      logoutNote = { text: "Logged out. The login is deleted from this Mac.", tone: "good" };
    } catch (error) {
      logoutNote = { text: failure(error), tone: "bad" };
    } finally {
      busy = null;
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
  <form onsubmit={enter}>
    <Section title="Log in">
      <Row title="E-mail">
        <input type="email" bind:value={email} autocomplete="username" spellcheck="false" />
      </Row>
      <Row title="Password">
        <input type="password" bind:value={password} autocomplete="current-password" />
      </Row>
      <Row title="Log in" note={loginNote?.text ?? actions.logIn.note} tone={loginNote?.tone ?? "secondary"}>
        <button type="submit" class="button strong" disabled={!actions.logIn.enabled}>Log in</button>
      </Row>
    </Section>
  </form>
  <Section>
    <Row
      title="Log out"
      note={logoutNote?.text ?? "Deletes the login from this Mac and forgets the account. The notes stay where they are."}
      tone={logoutNote?.tone ?? "secondary"}
    >
      <button type="button" class="button" disabled={!actions.logOut.enabled} onclick={leave}>Log out</button>
    </Row>
  </Section>
</Form>

<style>
  /* The form only groups the fields for Return; the page keeps its layout. */
  form {
    display: contents;
  }
</style>
