import { describe, expect, it } from "vitest";
import { mcpConfig, pathWarning } from "../settings/mcp";

describe("the MCP config", () => {
  it("mcp_config_names_the_sidecar_and_the_data_folder", () => {
    const text = mcpConfig("/Applications/Notedown.app/Contents/MacOS/notedown", "/Users/me/notedown-data");
    expect(JSON.parse(text)).toEqual({
      mcpServers: {
        notedown: {
          command: "/Applications/Notedown.app/Contents/MacOS/notedown",
          args: ["mcp", "--data", "/Users/me/notedown-data"],
        },
      },
    });
  });

  it("keeps quotes and spaces in the paths valid JSON", () => {
    const text = mcpConfig('/Applications/My "Apps"/Notedown.app/Contents/MacOS/notedown', "/Users/me/My Notes");
    expect(JSON.parse(text).mcpServers.notedown.args[2]).toBe("/Users/me/My Notes");
  });

  it("a_translocated_path_warns", () => {
    const translocated =
      "/private/var/folders/xy/abc/T/AppTranslocation/1234/d/Notedown.app/Contents/MacOS/notedown";
    expect(pathWarning(translocated)).toMatch(/Applications/);
    expect(pathWarning(translocated)).toMatch(/temporary/);
    // The same place without /private, and a translocated mount elsewhere.
    expect(pathWarning("/var/folders/xy/abc/T/AppTranslocation/1234/d/Notedown.app/Contents/MacOS/notedown")).toMatch(
      /temporary/,
    );
    expect(pathWarning("/Volumes/x/AppTranslocation/1234/d/Notedown.app/Contents/MacOS/notedown")).toMatch(/temporary/);
    expect(pathWarning("/Users/me/Downloads/Notedown.app/Contents/MacOS/notedown")).toMatch(/not in Applications/);
    expect(pathWarning("/Applications/Notedown.app/Contents/MacOS/notedown")).toBeNull();
  });

  it("uses no em or en dash", () => {
    expect(pathWarning("/private/var/folders/x/notedown") ?? "").not.toMatch(/[\u2013\u2014]/);
    expect(pathWarning("/tmp/notedown") ?? "").not.toMatch(/[\u2013\u2014]/);
  });
});
