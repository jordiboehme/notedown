<script lang="ts">
  import NameField from "./NameField.svelte";
  import type { EntryActions } from "./actions";

  // The actions on files: New Folder in the shown folder; Rename, Delete and Download on the
  // picked row. A new folder is named in place.
  let {
    actions,
    naming,
    onNewFolder,
    onNamed,
    onRename,
    onDelete,
    onDownload,
  }: {
    actions: EntryActions;
    naming: boolean;
    onNewFolder: () => void;
    onNamed: (typed: string | null) => void;
    onRename: () => void;
    onDelete: () => void;
    onDownload: () => void;
  } = $props();
</script>

<div class="actions" role="toolbar" aria-label="File actions">
  {#if naming}
    <span class="naming">
      <NameField name="untitled folder" label="Name of the new folder" onDone={onNamed} />
    </span>
  {:else}
    <button type="button" class="button" disabled={!actions.newFolder} onclick={onNewFolder}>New Folder</button>
  {/if}
  <button type="button" class="button" disabled={!actions.rename} onclick={onRename}>Rename</button>
  <button type="button" class="button" disabled={!actions.delete} onclick={onDelete}>Delete</button>
  <button type="button" class="button" disabled={!actions.download} onclick={onDownload}>Download</button>
</div>

<style>
  .actions {
    flex: none;
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 5px 14px;
    border-bottom: 1px solid var(--separator);
  }

  .naming {
    display: flex;
    width: 220px;
  }

  .button:disabled {
    color: var(--tertiary);
  }
</style>
