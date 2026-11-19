import { describe, expect, it } from "vitest";

/** The source of every Svelte file of the windows, by path. */
const sources = import.meta.glob<string>(["../settings/**/*.svelte", "../library/**/*.svelte", "../panel/**/*.svelte", "../lib/**/*.svelte"], {
  query: "?raw",
  import: "default",
  eager: true,
});

describe("the windows' pages", () => {
  it("hold no link the web view could load: the browser opens them (open_link)", () => {
    const files = Object.entries(sources);
    expect(files.length).toBeGreaterThan(20);
    for (const [path, source] of files) expect(source, path).not.toMatch(/\bhref\s*=/);
  });
});
