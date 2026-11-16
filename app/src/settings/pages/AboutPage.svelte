<script lang="ts">
  import icon from "../../../src-tauri/icons/128x128@2x.png";
  import { ABOUT } from "../about";

  // Settings > About, centred like Roger's: icon, name, version, motto, the two links and the
  // credits. The texts come from about.ts.
  let { version, onOpen }: { version: string | null; onOpen: (url: string) => void } = $props();
</script>

<div class="about">
  <span class="spacer"></span>
  <img class="icon" src={icon} alt="" width="128" height="128" />
  <div class="name">
    <h1>{ABOUT.name}</h1>
    {#if version}<p class="version">{ABOUT.version(version)}</p>{/if}
  </div>
  <p class="motto">{ABOUT.motto}</p>
  <div class="links">
    {#each ABOUT.links as link (link.url)}
      <button type="button" class="button" class:support={link.strong} onclick={() => onOpen(link.url)}>
        {#if link.strong}
          <svg viewBox="0 0 24 24" aria-hidden="true"
            ><path
              class="fill"
              d="M12 20.5s-7.5-4.6-7.5-10.2A4.3 4.3 0 0 1 12 7.6a4.3 4.3 0 0 1 7.5 2.7c0 5.6-7.5 10.2-7.5 10.2z"
            /></svg
          >
        {:else}
          <svg viewBox="0 0 24 24" aria-hidden="true"
            ><path d="M5 4h14a1 1 0 0 1 1 1v14a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1z M10 8h6v6 M16 8l-8 8" /></svg
          >
        {/if}
        {link.label}
      </button>
    {/each}
  </div>
  <span class="spacer"></span>
  <footer>
    {#each ABOUT.footer as line (line)}<p>{line}</p>{/each}
  </footer>
</div>

<style>
  .about {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 20px;
    min-height: 100%;
    box-sizing: border-box;
    padding: 16px 16px 28px;
    text-align: center;
  }

  .spacer {
    flex: 1;
  }

  .icon {
    width: 128px;
    height: 128px;
    filter: drop-shadow(0 4px 8px rgba(0, 0, 0, 0.15));
  }

  .name {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  h1 {
    margin: 0;
    font-size: 22px;
    font-weight: 600;
  }

  .version {
    margin: 0;
    font-size: 10px;
    color: var(--secondary);
  }

  .motto {
    margin: 0;
    font-size: 12px;
    color: var(--secondary);
  }

  .links {
    display: flex;
    gap: 12px;
  }

  .links .button {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    padding: 3px 12px;
  }

  .links .support {
    color: #fff;
    background: var(--pink);
    border-color: transparent;
  }

  svg {
    width: 13px;
    height: 13px;
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  svg .fill {
    fill: currentColor;
    stroke: none;
  }

  footer {
    display: flex;
    flex-direction: column;
    gap: 10px;
    font-size: 10px;
    color: var(--tertiary);
  }

  footer p {
    margin: 0;
  }
</style>
