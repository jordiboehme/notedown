<script lang="ts">
  // A name typed in place: Return or leaving the field takes it, Escape gives up. The name
  // before the extension is selected, as in the Finder.
  let { name, label, onDone }: { name: string; label: string; onDone: (typed: string | null) => void } = $props();

  let ended = false;

  function end(typed: string | null) {
    if (ended) return;
    ended = true;
    onDone(typed);
  }

  function start(field: HTMLInputElement) {
    field.focus();
    const dot = field.value.lastIndexOf(".");
    field.setSelectionRange(0, dot > 0 ? dot : field.value.length);
  }

  function key(event: KeyboardEvent & { currentTarget: HTMLInputElement }) {
    if (event.key === "Enter") end(event.currentTarget.value);
    else if (event.key === "Escape") end(null);
    else return;
    event.preventDefault();
    event.stopPropagation();
  }
</script>

<input
  class="field"
  type="text"
  value={name}
  aria-label={label}
  spellcheck="false"
  autocomplete="off"
  {@attach start}
  onkeydown={key}
  onblur={(event) => end(event.currentTarget.value)}
  onpointerdown={(event) => event.stopPropagation()}
  onclick={(event) => event.stopPropagation()}
  ondblclick={(event) => event.stopPropagation()}
/>

<style>
  .field {
    flex: 1;
    min-width: 0;
    font: inherit;
    color: var(--primary);
    padding: 0 3px;
    border: 1px solid var(--accent);
    border-radius: 3px;
    background: var(--box);
    user-select: text;
    outline: none;
  }
</style>
