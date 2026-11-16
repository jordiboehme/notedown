<script lang="ts">
  import Form from "../kit/Form.svelte";
  import Row from "../kit/Row.svelte";
  import Section from "../kit/Section.svelte";

  // Settings > Setup: a later version adds the setup steps here. Until then the Server page
  // sets where the server runs.
  let { onServer }: { onServer: () => void } = $props();
</script>

<Form>
  <Section title="Setup">
    <Row title="Setup steps" note="A later version guides you through the setup here." />
    <Row title="Where the server runs" note="Choose this Mac or another machine on the Server page.">
      <button type="button" class="button strong" onclick={onServer}>Open Server</button>
    </Row>
  </Section>
</Form>
