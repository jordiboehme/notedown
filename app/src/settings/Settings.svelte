<script lang="ts">
  import { onMount } from "svelte";
  import { getVersion } from "@tauri-apps/api/app";
  import { listen } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { createLastPage, firstPage } from "../lib/lastPage";
  import { startStatus } from "../lib/status.svelte";
  import {
    SETTINGS_PAGE_EVENT,
    UPDATE_STATE_EVENT,
    checkForUpdate,
    failure,
    installUpdateAndRestart,
    openLink,
    updateState,
  } from "./api";
  import { loadConfig } from "./config.svelte";
  import PageIcon from "./kit/PageIcon.svelte";
  import { GROUPS, isPageId, pageById, type PageId } from "./pages";
  import AboutPage from "./pages/AboutPage.svelte";
  import AccountPage from "./pages/AccountPage.svelte";
  import AgentsPage from "./pages/AgentsPage.svelte";
  import GeneralPage from "./pages/GeneralPage.svelte";
  import NotificationsPage from "./pages/NotificationsPage.svelte";
  import OcrPage from "./pages/OcrPage.svelte";
  import ServerPage from "./pages/ServerPage.svelte";
  import SetupPage from "./pages/SetupPage.svelte";
  import SupernotePage from "./pages/SupernotePage.svelte";
  import type { UpdateState } from "./update";
  import "../lib/system.css";
  import "./settings.css";

  // The settings window: the sidebar with the grouped pages on the left, the page on the
  // right; the window title is the page name (Roger's settings). The last page is
  // remembered; Rust can ask for a page (Set up in the panel).
  function storage() {
    try {
      return window.localStorage;
    } catch {
      return { getItem: () => null, setItem: () => {} };
    }
  }
  const last = createLastPage(storage());
  const requested = (window as unknown as { __NOTEDOWN_PAGE__?: string | null }).__NOTEDOWN_PAGE__;

  let page = $state<PageId>(firstPage(requested, last.load()));
  let now = $state(Date.now());
  let version = $state<string | null>(null);
  let update = $state<UpdateState>({ kind: "idle" });

  function show(next: PageId) {
    page = next;
    last.save(next);
  }

  $effect(() => {
    const title = pageById(page).title;
    getCurrentWindow()
      .setTitle(title)
      .catch((error: unknown) => console.error("notedown: the title could not be set", error));
  });

  onMount(() => {
    void loadConfig();
    getVersion()
      .then((v) => (version = v))
      .catch(() => {});
    const stops = [
      startStatus(),
      listen<string | null>(SETTINGS_PAGE_EVENT, (event) => {
        if (isPageId(event.payload)) show(event.payload);
      }),
      followUpdates(),
    ];
    const clock = setInterval(() => (now = Date.now()), 30_000);
    return () => {
      clearInterval(clock);
      for (const stop of stops) void stop.then((unlisten) => unlisten());
    };
  });

  // Rust may have told a state before this window listened (it opens the window for a found
  // update), so the state is read once the listener is there.
  async function followUpdates() {
    const unlisten = await listen<UpdateState>(UPDATE_STATE_EVENT, (event) => (update = event.payload));
    updateState()
      .then((state) => (update = state))
      .catch((error: unknown) => console.error("notedown: the update state could not be read", error));
    return unlisten;
  }

  // The answer comes as update-state too; a failure there is already shown.
  function checkUpdate() {
    checkForUpdate().catch((error: unknown) => console.error("notedown: the update check failed", failure(error)));
  }

  // On success Notedown ends and starts again; a failure comes as update-state.
  function installUpdate() {
    installUpdateAndRestart().catch((error: unknown) =>
      console.error("notedown: the update was not installed", failure(error)),
    );
  }

  function refresh() {
    now = Date.now();
    void loadConfig();
  }

  function open(url: string) {
    openLink(url).catch((error: unknown) => console.error("notedown: the link did not open", error));
  }
</script>

<svelte:window onfocus={refresh} />

<div class="settings window">
  <nav class="sidebar" aria-label="Settings">
    {#each GROUPS as group, i (i)}
      <div class="group">
        {#if group.title}<h2>{group.title}</h2>{/if}
        <ul>
          {#each group.pages as entry (entry.id)}
            <li>
              <button
                type="button"
                class="entry"
                aria-current={entry.id === page ? "page" : undefined}
                onclick={() => show(entry.id)}
              >
                <PageIcon page={entry} />
                <span>{entry.title}</span>
              </button>
            </li>
          {/each}
        </ul>
      </div>
    {/each}
  </nav>
  <main class="detail">
    {#key page}
      {#if page === "general"}<GeneralPage />
      {:else if page === "notifications"}<NotificationsPage />
      {:else if page === "supernote"}<SupernotePage {now} onSetup={() => show("setup")} />
      {:else if page === "agents"}<AgentsPage {now} />
      {:else if page === "ocr"}<OcrPage {now} />
      {:else if page === "server"}<ServerPage />
      {:else if page === "account"}<AccountPage />
      {:else if page === "setup"}<SetupPage onOpen={open} />
      {:else}<AboutPage {version} {update} onOpen={open} onCheck={checkUpdate} onInstall={installUpdate} />
      {/if}
    {/key}
  </main>
</div>

<style>
  .window {
    display: flex;
    height: 100%;
    user-select: none;
  }

  .sidebar {
    flex: none;
    width: 200px;
    padding: 8px 10px;
    overflow-y: auto;
    box-sizing: border-box;
    background: var(--sidebar);
    border-right: 1px solid var(--separator);
  }

  .group + .group {
    margin-top: 10px;
  }

  h2 {
    margin: 4px 8px 2px;
    font-size: 11px;
    font-weight: 600;
    color: var(--secondary);
  }

  ul {
    list-style: none;
    margin: 0;
    padding: 0;
  }

  .entry {
    display: flex;
    align-items: center;
    gap: 7px;
    width: 100%;
    padding: 4px 6px;
    font: inherit;
    color: var(--primary);
    text-align: left;
    border: none;
    border-radius: 6px;
    background: transparent;
  }

  .entry[aria-current="page"] {
    color: var(--on-brand);
    background: var(--brand);
  }

  .entry:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }

  .detail {
    flex: 1;
    min-width: 0;
    overflow-y: auto;
  }
</style>
