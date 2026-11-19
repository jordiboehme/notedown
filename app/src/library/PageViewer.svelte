<script lang="ts">
  import type { PageImage } from "./session.svelte";
  import type { ReviewPage } from "./review";
  import { ACTUAL, ZOOMS, stepPage, viewerInput, zoomIn, zoomOut } from "./zoom";

  // One page alone in a large view over the whole window, fit to the window. The same zoom
  // keys as the pages side; the arrow keys and Page Up and Page Down go to the page before or
  // after, Escape and the Close button end it. It shows what the session has; it asks for
  // nothing itself (`onStep` changes the page, the session asks for its image). A right click
  // on the page opens the page menu (`onPageMenu`).
  let {
    row,
    image,
    count,
    covered = false,
    onStep,
    onClose,
    onPageMenu = () => {},
  }: {
    row: ReviewPage;
    image: PageImage | undefined;
    count: number;
    /** A search lies over the review: no key reaches the page. */
    covered?: boolean;
    onStep: (index: number) => void;
    onClose: () => void;
    onPageMenu?: (event: MouseEvent, index: number) => void;
  } = $props();

  /** 1 fits the page to the window. */
  let zoom = $state(ACTUAL);
  let closeButton = $state<HTMLButtonElement>();

  $effect(() => {
    closeButton?.focus();
  });

  function key(event: KeyboardEvent) {
    const input = viewerInput(event, covered);
    if (!input) return;
    event.preventDefault();
    if ("zoom" in input) {
      const step = input.zoom;
      zoom = step === "in" ? zoomIn(zoom) : step === "out" ? zoomOut(zoom) : ACTUAL;
    } else if (input.move === "close") onClose();
    else onStep(stepPage(row.index, count, input.move));
  }
</script>

<svelte:window onkeydown={key} />

<div class="viewer" role="dialog" aria-modal="true" aria-label={row.label}>
  <header class="head">
    <p class="label">{row.label} of {count}</p>
    <div class="buttons">
      <button type="button" disabled={row.index === 0} onclick={() => onStep(stepPage(row.index, count, "prev"))}>Previous Page</button>
      <button type="button" disabled={row.index >= count - 1} onclick={() => onStep(stepPage(row.index, count, "next"))}>Next Page</button>
      <button type="button" disabled={zoom === ZOOMS[0]} onclick={() => (zoom = zoomOut(zoom))}>Zoom Out</button>
      <button type="button" disabled={zoom === ACTUAL} onclick={() => (zoom = ACTUAL)}>Actual Size</button>
      <button type="button" disabled={zoom === ZOOMS[ZOOMS.length - 1]} onclick={() => (zoom = zoomIn(zoom))}>Zoom In</button>
      <button type="button" bind:this={closeButton} onclick={onClose}>Close</button>
    </div>
  </header>
  <div class="stage">
    <!-- Fit to the window at 1: as wide as the stage or as high as it, whichever is less. -->
    <div class="paper" style:aspect-ratio={row.ratio} style:width={`min(${zoom * 100}cqw, ${zoom * 100 * row.ratio}cqh)`}>
      {#if image?.state === "shown"}
        <img src={image.url} alt={row.label} oncontextmenu={(event) => onPageMenu(event, row.index)} />
      {:else if image?.state === "failed"}
        <p class="why">{image.text}</p>
      {:else if image?.state === "loading"}
        <p class="why">Drawing the page...</p>
      {/if}
    </div>
  </div>
</div>

<style>
  .viewer {
    position: fixed;
    inset: 0;
    z-index: 10;
    display: flex;
    flex-direction: column;
    background: var(--window);
  }

  .head {
    flex: none;
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 6px 14px;
    border-bottom: 1px solid var(--separator);
  }

  .label {
    margin: 0;
    font-weight: 600;
    white-space: nowrap;
  }

  .buttons {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin-left: auto;
  }

  .buttons button {
    font: inherit;
    padding: 2px 10px;
    border-radius: 6px;
    border: 1px solid var(--button-rim);
    background: var(--button);
    color: inherit;
  }

  .buttons button:disabled {
    opacity: 0.5;
  }

  .stage {
    flex: 1;
    min-height: 0;
    display: flex;
    overflow: auto;
    padding: 12px;
    container-type: size;
  }

  .paper {
    box-sizing: border-box;
    flex: none;
    /* Centered while it fits; an auto margin is 0 once it is larger, so all of it scrolls. */
    margin: auto;
    display: flex;
    align-items: center;
    justify-content: center;
    background: #ffffff;
    border: 1px solid var(--separator);
  }

  .paper img {
    display: block;
    width: 100%;
    height: 100%;
    object-fit: contain;
  }

  .why {
    margin: 0;
    color: var(--secondary);
  }
</style>
