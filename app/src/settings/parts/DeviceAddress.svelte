<script lang="ts">
  import Banner from "../kit/Banner.svelte";
  import Row from "../kit/Row.svelte";

  // The addresses the Supernote reaches this Mac's server at, and the fixed-address notice.
  // The Server page and step 1 of the setup show it.
  let { urls }: { urls: string[] } = $props();
</script>

{#each urls as url (url)}
  <Row title={url} note="Enter this address in the private cloud settings of the Supernote." />
{:else}
  <Row title="No address yet" note="The address shows while the server runs." />
{/each}
<Banner message="Give this Mac a fixed address in your router. The Supernote finds the server only at this address." />
