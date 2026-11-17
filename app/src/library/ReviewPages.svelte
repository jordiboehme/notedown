<script lang="ts">
  import type { PageImage } from "./session.svelte";
  import { syncGuard, syncTop, type Offsets, type ReviewPage, type Side } from "./review";

  // The review's two sides: the handwritten pages and their Markdown, scrolled together page by
  // page. The Markdown is shown as text (pre-wrap), never as HTML. It tells which pages came
  // into view or left it (`onSeen`, all of one observer callback at once) and the size of each
  // loaded image (`onMeasured`); it asks for nothing itself.
  let {
    rows,
    images,
    onSeen,
    onMeasured,
  }: {
    rows: ReviewPage[];
    images: Record<number, PageImage>;
    onSeen: (changes: [index: number, visible: boolean][]) => void;
    onMeasured: (index: number, width: number, height: number) => void;
  } = $props();

  let pagesSide = $state<HTMLDivElement>();
  let textSide = $state<HTMLDivElement>();
  /** The value the window set on a side: its scroll event is not followed back. */
  const guard = syncGuard();
  /** The page tops of each side, measured again when a page changes its size. */
  const tops: Partial<Record<Side, Offsets>> = {};

  const count = $derived(rows.length);

  // Which pages are in view of the pages side; the session asks for the near ones. Made again
  // only when the pages change, not when an image's ratio becomes known.
  $effect(() => {
    const root = pagesSide;
    if (!root || count === 0) return;
    const observer = new IntersectionObserver(
      (entries) => {
        onSeen(entries.map((entry) => [Number((entry.target as HTMLElement).dataset.page), entry.isIntersecting]));
      },
      { root },
    );
    for (const page of root.querySelectorAll<HTMLElement>("[data-page]")) observer.observe(page);
    return () => observer.disconnect();
  });

  // A page that changes its size (an image's ratio became known, the window was resized) moves
  // the tops: they are measured again at the next scroll.
  $effect(() => {
    const sides = [pagesSide, textSide];
    if (sides.some((side) => !side) || count === 0) return;
    const resized = new ResizeObserver(() => {
      delete tops.pages;
      delete tops.text;
    });
    for (const side of sides) for (const page of side!.querySelectorAll("[data-page]")) resized.observe(page);
    return () => resized.disconnect();
  });

  /** The tops of a side's pages in its scroll area (each side is their offset parent). */
  function offsets(name: Side, side: HTMLDivElement): Offsets {
    let known = tops[name];
    if (!known) {
      const pages = [...side.querySelectorAll<HTMLElement>("[data-page]")];
      known = { tops: pages.map((p) => p.offsetTop), end: side.scrollHeight };
      tops[name] = known;
    }
    return known;
  }

  function follow(from: Side) {
    const to: Side = from === "pages" ? "text" : "pages";
    const fromSide = from === "pages" ? pagesSide : textSide;
    const toSide = to === "pages" ? pagesSide : textSide;
    if (!fromSide || !toSide) return;
    if (!guard.follows(from, fromSide.scrollTop)) return;
    const top = syncTop(fromSide.scrollTop, offsets(from, fromSide), offsets(to, toSide));
    if (Math.abs(toSide.scrollTop - top) < 1) return;
    toSide.scrollTop = top;
    // What the side took (it stops at its end), so its own scroll event matches.
    guard.set(to, toSide.scrollTop);
  }
</script>

<div class="sides">
  <div class="side pages" bind:this={pagesSide} onscroll={() => follow("pages")} aria-label="Pages">
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
  <div class="side text" bind:this={textSide} onscroll={() => follow("text")} aria-label="Text">
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
