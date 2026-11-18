<script lang="ts">
  import { tick } from "svelte";
  import type { PageImage } from "./session.svelte";
  import { keepsHit, syncGuard, syncTop, type Offsets, type ReviewPage, type Side } from "./review";
  import { clampSplit, keepPlace, openKey, readSplit, saveSplit } from "./zoom";

  // The review's two sides: the handwritten pages and their Markdown, scrolled together page by
  // page. The Markdown is shown as text (pre-wrap), never as HTML. It tells which pages came
  // into view or left it (`onSeen`, all of one observer callback at once) and the size of each
  // loaded image (`onMeasured`); it asks for nothing itself. The pages column is `zoom` times
  // the width of its side (scrolled sideways past 1). The divider between the sides can be
  // dragged; the split is kept per viewer. A double-click on a page opens it alone
  // (`onOpen`); `showPage` scrolls both sides to a page. The text side shows each block of a
  // page in its own `pre`; `showBlock` scrolls it to a block (a search hit), marks the block
  // until the text side scrolls away from it, and the pages side follows.
  let {
    rows,
    images,
    zoom = 1,
    onSeen,
    onMeasured,
    onOpen = () => {},
  }: {
    rows: ReviewPage[];
    images: Record<number, PageImage>;
    zoom?: number;
    onSeen: (changes: [index: number, visible: boolean][]) => void;
    onMeasured: (index: number, width: number, height: number) => void;
    onOpen?: (index: number) => void;
  } = $props();

  /** The viewer's storage; null where it cannot be used. */
  function storage(): Storage | null {
    try {
      return window.localStorage;
    } catch {
      return null;
    }
  }

  let sides = $state<HTMLDivElement>();
  /** The share of the pages side. */
  let split = $state(readSplit(typeof window === "undefined" ? null : storage()));
  let dragging = false;

  function drag(event: PointerEvent) {
    if (!dragging || !sides) return;
    const box = sides.getBoundingClientRect();
    if (box.width > 0) split = clampSplit((event.clientX - box.left) / box.width);
  }

  function release(event: PointerEvent) {
    if (!dragging) return;
    dragging = false;
    const handle = event.currentTarget as HTMLElement;
    if (handle.hasPointerCapture(event.pointerId)) handle.releasePointerCapture(event.pointerId);
    saveSplit(storage(), split);
  }

  function nudge(event: KeyboardEvent) {
    const step = event.key === "ArrowLeft" ? -0.05 : event.key === "ArrowRight" ? 0.05 : 0;
    if (step === 0) return;
    event.preventDefault();
    split = clampSplit(split + step);
    saveSplit(storage(), split);
  }

  let pagesSide = $state<HTMLDivElement>();
  let textSide = $state<HTMLDivElement>();
  /** The value the window set on a side: its scroll event is not followed back. */
  const guard = syncGuard();
  /** The page tops of each side, measured again when a page changes its size. */
  const tops: Partial<Record<Side, Offsets>> = {};

  const count = $derived(rows.length);

  // A zoom keeps the page in view at the same share of it: the place is taken before the
  // column changes its width and set again once the pages are laid out at the new size.
  // svelte-ignore state_referenced_locally
  let shownZoom = zoom;
  $effect.pre(() => {
    const now = zoom;
    const side = pagesSide;
    if (now === shownZoom || !side) {
      shownZoom = now;
      return;
    }
    const was = shownZoom;
    shownZoom = now;
    const before = offsets("pages", side);
    const top = side.scrollTop;
    const middle = side.scrollLeft + side.clientWidth / 2;
    void tick().then(() => {
      delete tops.pages;
      delete tops.text;
      const after = offsets("pages", side);
      side.scrollTop = keepPlace(top, before, after);
      side.scrollLeft = Math.max(0, (middle * now) / was - side.clientWidth / 2);
      guard.set("pages", side.scrollTop);
    });
  });

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

  /** Scrolls the pages side to the top of page `index`; the text side follows. */
  export function showPage(index: number) {
    const side = pagesSide;
    if (!side) return;
    const known = offsets("pages", side);
    const top = known.tops[Math.min(index, known.tops.length - 1)];
    if (top === undefined) return;
    side.scrollTop = top;
    follow("pages");
  }

  /** The block of a search hit, marked until the user scrolls: `index:blockId`. */
  let hit = $state<string | null>(null);
  /** Where `showBlock` put the text side; a scroll away from it ends the mark. */
  let hitTop = 0;

  /**
   * Scrolls the text side to the block `blockId` of page `index`, or to the page when the
   * block is null or not there; the pages side follows.
   */
  export function showBlock(index: number, blockId: string | null) {
    const side = textSide;
    if (!side) return;
    const page = side.querySelector<HTMLElement>(`[data-page="${index}"]`);
    if (!page) return;
    const block =
      blockId === null
        ? null
        : ([...page.querySelectorAll<HTMLElement>("[data-block]")].find((b) => b.dataset.block === blockId) ?? null);
    const target = block ?? page;
    // The side is the offset parent of its pages and blocks; keep its padding above.
    side.scrollTop = Math.max(0, target.offsetTop - 12);
    hitTop = side.scrollTop;
    hit = block ? `${index}:${blockId}` : null;
    follow("text");
  }

  /**
   * The text side scrolled. Away from where `showBlock` put it, the scroll is the user's
   * (wheel, keys, scroll bar, or the pages side it follows): the mark of the hit goes.
   */
  function textScrolled() {
    if (hit !== null && textSide && !keepsHit(hitTop, textSide.scrollTop)) hit = null;
    follow("text");
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

<div class="sides" bind:this={sides} style:grid-template-columns={`minmax(0, ${split}fr) 6px minmax(0, ${1 - split}fr)`}>
  <div class="side pages" bind:this={pagesSide} onscroll={() => follow("pages")} aria-label="Pages">
    <div class="column" style:width={`${zoom * 100}%`}>
    {#each rows as row (row.index)}
      {@const image = images[row.index]}
      <section class="page" data-page={row.index} aria-label={row.label}>
        <div
          class="paper"
          style:aspect-ratio={row.ratio}
          role="button"
          tabindex="0"
          aria-label={`Show ${row.label} alone`}
          title="Double-click to show the page alone"
          ondblclick={() => onOpen(row.index)}
          onkeydown={(event) => {
            if (!openKey(event.key)) return;
            event.preventDefault();
            onOpen(row.index);
          }}
        >
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
  </div>
  <!-- A focusable separator is a widget: it moves with the arrow keys too. -->
  <!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions -->
  <div
    class="divider"
    role="separator"
    aria-orientation="vertical"
    aria-label="Divider between pages and text"
    aria-valuemin={20}
    aria-valuemax={80}
    aria-valuenow={Math.round(split * 100)}
    tabindex="0"
    onpointerdown={(event) => {
      dragging = true;
      (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
      event.preventDefault();
    }}
    onpointermove={drag}
    onpointerup={release}
    onpointercancel={release}
    onkeydown={nudge}
  ></div>
  <div class="side text" bind:this={textSide} onscroll={textScrolled} aria-label="Text">
    {#each rows as row (row.index)}
      <section class="page" data-page={row.index} aria-label={row.label}>
        <p class="label">{row.label}</p>
        {#if row.stateText}<p class="why">{row.stateText}</p>{/if}
        {#each row.blocks as block (block.id)}
          {#if block.text.trim() !== ""}
            <pre class="markdown" class:hit={hit === `${row.index}:${block.id}`} data-block={block.id}>{block.text}</pre>
          {/if}
        {/each}
      </section>
    {/each}
  </div>
</div>

<style>
  .sides {
    flex: 1;
    display: grid;
    min-height: 0;
  }

  .side {
    position: relative;
    min-height: 0;
    overflow-y: auto;
    padding: 12px 16px;
  }

  .pages {
    overflow-x: auto;
    background: var(--window);
  }

  .divider {
    cursor: col-resize;
    touch-action: none;
    background: var(--window);
    border-left: 1px solid var(--separator);
    border-right: 1px solid var(--separator);
  }

  .divider:focus-visible {
    outline: 2px solid var(--accent, Highlight);
    outline-offset: -2px;
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
    background: #ffffff;
    border: 1px solid var(--separator);
    border-radius: 3px;
    overflow: hidden;
  }

  .paper:focus-visible {
    outline: 2px solid var(--accent, Highlight);
    outline-offset: 2px;
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
    margin: 0 0 0.8em;
    font: inherit;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    user-select: text;
    cursor: text;
  }

  /* The block of a search hit, until the user scrolls. */
  .markdown.hit {
    border-radius: 3px;
    background: color-mix(in srgb, var(--accent), transparent 80%);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent), transparent 80%);
  }
</style>
