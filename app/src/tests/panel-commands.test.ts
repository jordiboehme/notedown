import { describe, expect, it } from "vitest";
import { pauseCalls } from "../panel/commands";
import build from "../../src-tauri/build.rs?raw";
import panelCapability from "../../src-tauri/capabilities/panel.json";

/** The names in build.rs's COMMANDS: the commands Rust registers (lib.rs tests keep
 * generate_handler! equal to it). */
function registered(): Set<string> {
  const list = build.slice(build.indexOf("const COMMANDS"), build.indexOf("];"));
  return new Set([...list.matchAll(/"([a-z_]+)"/g)].map((m) => m[1]));
}

/** The app commands the panel window may call, from its capability file. */
function grantedToPanel(): Set<string> {
  const permissions: string[] = panelCapability.permissions;
  return new Set(permissions.filter((p) => p.startsWith("allow-")).map((p) => p.slice(6).replaceAll("-", "_")));
}

describe("the panel's pause calls", () => {
  function sent() {
    const calls: [string, Record<string, unknown> | undefined][] = [];
    const send = pauseCalls((command, args) => calls.push([command, args]));
    send.pauseMenu(12, 40.5);
    send.resumeOcr();
    return calls;
  }

  it("send the pause menu with its point and Resume OCR Now", () => {
    expect(sent()).toEqual([
      ["panel_pause_menu", { x: 12, y: 40.5 }],
      ["panel_ocr_resume", undefined],
    ]);
  });

  it("name commands that Rust registers and grants to the panel", () => {
    const names = sent().map(([command]) => command);
    for (const command of names) {
      expect(registered(), command).toContain(command);
      expect(grantedToPanel(), command).toContain(command);
    }
    // The reading of the files works: a typo is not in them.
    expect(registered()).not.toContain("panel_pause_menus");
    expect(registered()).toContain("ocr_resume");
  });
});
