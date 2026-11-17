import { describe, expect, it } from "vitest";
import { render } from "svelte/server";
import { ABOUT } from "../settings/about";
import AboutPage from "../settings/pages/AboutPage.svelte";

describe("the About page", () => {
  it("about_has_the_motto_links_and_footer", () => {
    expect(ABOUT.name).toBe("Notedown");
    expect(ABOUT.motto).toBe("Written by hand. Read by machine.");
    expect(ABOUT.links).toEqual([
      { label: "Support Me", url: "https://ko-fi.com/V7V31T6CL9", strong: true },
      { label: "GitHub", url: "https://github.com/jordiboehme/notedown", strong: false },
    ]);
    expect(ABOUT.footer).toEqual(["Powered by llama.cpp and Chandra", "Created with love by Jordi Böhme, AGPL-3.0"]);
    expect(ABOUT.version("0.1.0")).toBe("Version 0.1.0");
  });

  it("renders what about.ts says", () => {
    const body = render(AboutPage, {
      props: { version: "0.1.0", update: { kind: "idle" }, onOpen: () => {}, onCheck: () => {}, onInstall: () => {} },
    }).body;
    for (const text of [ABOUT.name, ABOUT.motto, "Version 0.1.0", ...ABOUT.footer]) {
      expect(body).toContain(text);
    }
    for (const link of ABOUT.links) expect(body).toContain(link.label);
    expect(body).not.toMatch(/[\u2013\u2014]/);
  });
});
