import { mount, type Component } from "svelte";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import "./app.css";
import Library from "./library/Library.svelte";
import PanelWindow from "./panel/PanelWindow.svelte";
import Settings from "./settings/Settings.svelte";

// One page for every window: the window label picks the view.
const views: Record<string, Component> = {
  panel: PanelWindow,
  library: Library,
  settings: Settings,
};

const app = mount(views[getCurrentWebviewWindow().label] ?? PanelWindow, {
  target: document.getElementById("app")!,
});

export default app;
