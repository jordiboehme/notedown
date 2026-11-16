<script lang="ts">
  import { onMount } from "svelte";
  import { failure, getAutostart, setAutostart, setStartView, type StartView } from "../api";
  import { config } from "../config.svelte";
  import Form from "../kit/Form.svelte";
  import Row from "../kit/Row.svelte";
  import Section from "../kit/Section.svelte";
  import Segmented from "../kit/Segmented.svelte";
  import Switch from "../kit/Switch.svelte";

  // Settings > General: start at login and the folder view of the library.
  let autostart = $state<boolean | null>(null);
  let problem = $state<string | null>(null);

  onMount(() => {
    getAutostart()
      .then((on) => (autostart = on))
      .catch((error: unknown) => (problem = failure(error)));
  });

  async function toggle(on: boolean) {
    try {
      autostart = await setAutostart(on);
      problem = null;
    } catch (error) {
      problem = failure(error);
    }
  }

  async function view(next: StartView) {
    try {
      config.current = await setStartView(next);
    } catch (error) {
      problem = failure(error);
    }
  }
</script>

<Form>
  <Section>
    <Row title="Start at login" note={problem ?? "Notedown opens in the menu bar when you log in."} tone={problem ? "bad" : "secondary"}>
      <Switch checked={autostart ?? false} disabled={autostart === null} label="Start at login" onchange={toggle} />
    </Row>
    <Row title="Folder view" note="How the library shows folders when it opens.">
      <Segmented
        label="Folder view"
        value={config.current?.startView ?? "tree"}
        options={[
          { value: "tree", label: "Tree" },
          { value: "columns", label: "Columns" },
        ]}
        disabled={config.current === null}
        onchange={view}
      />
    </Row>
  </Section>
</Form>
