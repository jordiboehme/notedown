import { mount, type Component } from "svelte";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import "./app.css";
import App from "./App.svelte";
import Library from "./library/Library.svelte";
import Settings from "./settings/Settings.svelte";

// One page for every window: the window label picks the view.
const views: Record<string, Component> = {
  panel: App,
  library: Library,
  settings: Settings,
};

const app = mount(views[getCurrentWebviewWindow().label] ?? App, {
  target: document.getElementById("app")!,
});

export default app;
