<script lang="ts">
  import type { Status } from "../../lib/api";
  import { ACCOUNT_MADE, type ConfigAnswer, createAccount, failure, SETUP_LINKS } from "../api";
  import { logInFlow, type LogInOutcome } from "../account";
  import { accountCommand, createAccountProblem } from "../checklist";
  import { config } from "../config.svelte";
  import Row from "../kit/Row.svelte";
  import Segmented from "../kit/Segmented.svelte";
  import { slugFromEmail } from "../slug";

  // Step 2 of the setup: log in; on this Mac's server also create an account; for a server on
  // another machine the command that makes the account there.
  let {
    email = $bindable(),
    existing,
    onLoggedIn,
    onOpen,
  }: {
    email: string;
    existing: Status["existing"];
    onLoggedIn: (outcome: LogInOutcome) => void;
    onOpen: (url: string) => void;
  } = $props();

  type Form = "logIn" | "create";
  type Note = { text: string; tone: "secondary" | "good" | "bad" };

  let form = $state<Form>("logIn");
  let slug = $state("");
  let slugEdited = $state(false);
  let password = $state("");
  let repeat = $state("");
  let busy = $state(false);
  let note = $state<Note | null>(null);
  let copied = $state<Note | null>(null);

  const mode = $derived(config.current?.mode ?? "notSet");
  const shownSlug = $derived(slugEdited ? slug : slugFromEmail(email));
  const problem = $derived(
    form === "create" ? createAccountProblem({ email, slug: shownSlug, password, repeat }) : null,
  );
  const filled = $derived(email.trim() !== "" && password !== "");
  const command = $derived(accountCommand(email));

  async function run(work: () => Promise<void>) {
    busy = true;
    note = null;
    try {
      await work();
    } catch (error) {
      const text = failure(error);
      if (text === ACCOUNT_MADE) {
        // The account is there; only the login after it failed. Log in with the same e-mail.
        form = "logIn";
        note = { text, tone: "secondary" };
      } else {
        note = { text, tone: "bad" };
      }
    } finally {
      password = "";
      repeat = "";
      busy = false;
    }
  }

  function done(answer: ConfigAnswer, outcome: LogInOutcome) {
    config.current = answer.config;
    note = answer.keychain
      ? { text: `Logged in as ${answer.config.account}. ${answer.keychain}`, tone: "bad" }
      : { text: `Logged in as ${answer.config.account}.`, tone: "good" };
    onLoggedIn(outcome);
  }

  function submit(event: SubmitEvent) {
    event.preventDefault();
    const cfg = config.current;
    if (busy || cfg === null) return;
    if (form === "create") {
      if (problem !== null) return;
      const name = shownSlug.trim();
      void run(async () => {
        const answer = await createAccount(name, email.trim(), password);
        done(answer, { loggedIn: true, answer, connectError: null });
      });
      return;
    }
    if (!filled) return;
    void run(async () => {
      const outcome = await logInFlow(email.trim(), password, cfg, existing);
      done(outcome.answer, outcome);
    });
  }

  async function copy() {
    try {
      await navigator.clipboard.writeText(command);
      copied = { text: "Copied. Run it on the other machine. It asks for the password.", tone: "good" };
    } catch {
      copied = { text: "Copying did not work. Select the command below and copy it.", tone: "bad" };
    }
  }
</script>

{#if mode === "local"}
  <Row title="Account" note="Log in to an account of this server, or create one.">
    <Segmented
      label="Log in or create an account"
      value={form}
      options={[
        { value: "logIn", label: "Log in" },
        { value: "create", label: "Create an account" },
      ]}
      onchange={(next) => {
        form = next;
        note = null;
      }}
    />
  </Row>
{/if}
<form onsubmit={submit}>
  <Row title="E-mail" note="The same e-mail the Supernote uses.">
    <input type="email" bind:value={email} autocomplete="username" spellcheck="false" />
  </Row>
  {#if form === "create" && mode === "local"}
    <Row title="Account name" note="Made from the e-mail. You can change it.">
      <input
        type="text"
        value={shownSlug}
        oninput={(event) => {
          slugEdited = true;
          slug = event.currentTarget.value;
        }}
        spellcheck="false"
      />
    </Row>
  {/if}
  <Row title="Password">
    <input
      type="password"
      bind:value={password}
      autocomplete={form === "create" ? "new-password" : "current-password"}
    />
  </Row>
  {#if form === "create" && mode === "local"}
    <Row title="Repeat the password">
      <input type="password" bind:value={repeat} autocomplete="new-password" />
    </Row>
    <Row
      title="Create the account"
      note={note?.text ?? (password !== "" ? problem : null) ?? "Notedown makes the account and logs in with it."}
      tone={note?.tone ?? (password !== "" && problem ? "bad" : "secondary")}
    >
      <button type="submit" class="button strong" disabled={busy || problem !== null}>Create account</button>
    </Row>
  {:else}
    <Row
      title="Log in"
      note={note?.text ?? "Notedown keeps a write token in the Keychain of this Mac."}
      tone={note?.tone ?? "secondary"}
    >
      <button type="submit" class="button strong" disabled={busy || !filled || mode === "notSet"}>Log in</button>
    </Row>
  {/if}
</form>
{#if mode === "remote"}
  <Row
    title="No account yet?"
    note={copied?.text ?? "Make the account on the other machine with this command."}
    tone={copied?.tone ?? "secondary"}
  >
    <button type="button" class="button" onclick={copy}>Copy</button>
    <button type="button" class="button" onclick={() => onOpen(SETUP_LINKS.otherMachine)}>
      The server on another machine
    </button>
  </Row>
  <pre class="log">{command}</pre>
{/if}

<style>
  /* The form only groups the fields for Return; the rows keep their layout. */
  form {
    display: contents;
  }
</style>
