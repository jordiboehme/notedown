// "Copy MCP config" on the Agents page: the config an agent needs to start Notedown's MCP
// server, and a warning when the path in it will not last (review M7).

/** `{"mcpServers":{"notedown":{"command":<sidecar>,"args":["mcp","--data",<dataDir>]}}}` */
export function mcpConfig(sidecar: string, dataDir: string): string {
  const config = {
    mcpServers: {
      notedown: { command: sidecar, args: ["mcp", "--data", dataDir] },
    },
  };
  return JSON.stringify(config, null, 2);
}

/**
 * A warning when the program path will change: macOS runs an app that was opened from
 * Downloads from a temporary place (translocation), and an app outside Applications may move.
 */
export function pathWarning(sidecar: string): string | null {
  if (sidecar.startsWith("/private/var/folders/")) {
    return "Notedown runs from a temporary place, so this path will not work for long. Move Notedown to Applications, open it from there and copy the config again.";
  }
  if (!sidecar.startsWith("/Applications/")) {
    return "Notedown is not in Applications. If you move it, copy the config again.";
  }
  return null;
}
