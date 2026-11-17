<script lang="ts">
  import type { PageImage } from "./session.svelte";
  import { syncTop, type Offsets, type ReviewPage } from "./review";

  // The review's two sides: the handwritten pages and their Markdown, scrolled together page by
  // page. The Markdown is shown as text (pre-wrap), never as HTML. It tells which pages are in
  // view (`onSeen`) and the size of each loaded image (`onMeasured`); it asks for nothing itself.
  let {
    rows,
    images,
    onSeen,
    onMeasured,
  }: {
    rows: ReviewPage[];
    images: Record<number, PageImage>;
    onSeen: (index: number, visible: boolean) => void;
    onMeasured: (index: number, width: number, height: number) => void;
  } = $props();

  let pagesSide = $state<HTMLDivElement>();
  let textSide = $state<HTMLDivElement>();
  /** The side the window scrolled itself: its next scroll event is not followed back. */
  let driven: HTMLDivElement | null = null;

  const count = $derived(rows.length);

  // Which pages are in view of the pages side; the session asks for the near ones. Made again
  // only when the pages change, not when an image's ratio becomes known.
  $effect(() => {
    const root = pagesSide;
    if (!root || count === 0) return;
    const observer = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          const index = Number((entry.target as HTMLElement).dataset.page);
          onSeen(index, entry.isIntersecting);
        }
      },
      { root },
    );
    for (const page of root.querySelectorAll<HTMLElement>("[data-page]")) observer.observe(page);
    return () => observer.disconnect();
  });

  /** The tops of a side's pages in its scroll area (each side is their offset parent). */
  function offsets(side: HTMLDivElement): Offsets {
    const pages = [...side.querySelectorAll<HTMLElement>("[data-page]")];
    return { tops: pages.map((p) => p.offsetTop), end: side.scrollHeight };
  }

  function follow(from: HTMLDivElement | undefined, to: HTMLDivElement | undefined) {
    if (!from || !to) return;
    if (driven === from) {
      driven = null;
      return;
    }
    const top = syncTop(from.scrollTop, offsets(from), offsets(to));
    if (Math.abs(to.scrollTop - top) < 1) return;
    driven = to;
    to.scrollTop = top;
    requestAnimationFrame(() => {
      if (driven === to) driven = null;
    });
  }
</script>

<div class="sides">
  <div class="side pages" bind:this={pagesSide} onscroll={() => follow(pagesSide, textSide)} aria-label="Pages">
    {#each rows as row (row.index)}
      {@const image = images[row.index]}
      <section class="page" data-page={row.index} aria-label={row.label}>
        <div class="paper" style:aspect-ratio={row.ratio}>
          {#if image?.state === "shown"}
            <img
              src={image.url}
              alt={row.label}
              onload={(event) => {
                const img = event.currentTarget as HTMLImageElement;
                onMeasured(row.index, img.naturalWidth, img.naturalHeight);
              }}
            />
          {:else if image?.state === "failed"}
            <p class="why">{image.text}</p>
          {:else if image?.state === "loading"}
            <p class="why">Drawing the page...</p>
          {/if}
        </div>
        <p class="label">{row.label}</p>
      </section>
    {/each}
  </div>
  <div class="side text" bind:this={textSide} onscroll={() => follow(textSide, pagesSide)} aria-label="Text">
    {#each rows as row (row.index)}
      <section class="page" data-page={row.index} aria-label={row.label}>
        <p class="label">{row.label}</p>
        {#if row.stateText}<p class="why">{row.stateText}</p>{/if}
        {#if row.markdown.trim() !== ""}<pre class="markdown">{row.markdown}</pre>{/if}
      </section>
    {/each}
  </div>
</div>

<style>
  .sides {
    flex: 1;
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
    min-height: 0;
  }

  .side {
    position: relative;
    min-height: 0;
    overflow-y: auto;
    padding: 12px 16px;
  }

  .pages {
    border-right: 1px solid var(--separator);
    background: var(--window);
  }

  .page {
    margin: 0 0 16px;
  }

  .text .page {
    padding-bottom: 12px;
    border-bottom: 1px solid var(--separator);
  }

  .paper {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 100%;
    background: #fdfcf8;
    border: 1px solid var(--separator);
    border-radius: 3px;
    overflow: hidden;
  }

  .paper img {
    display: block;
    width: 100%;
    height: 100%;
    object-fit: contain;
  }

  .label {
    margin: 4px 0;
    font-size: 11px;
    color: var(--secondary);
  }

  .why {
    margin: 4px 8px;
    color: var(--secondary);
  }

  .markdown {
    margin: 0;
    font: inherit;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    user-select: text;
    cursor: text;
  }

  @media (prefers-color-scheme: dark) {
    .paper {
      background: #2a2925;
    }
  }
</style>
