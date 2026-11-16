import { describe, expect, it } from "vitest";
import { GROUPS, PAGES, isPageId, pageById } from "../settings/pages";

describe("the settings pages", () => {
  it("pages_are_in_the_decided_order_and_groups", () => {
    expect(PAGES.map((p) => p.title)).toEqual([
      "General",
      "Notifications",
      "Supernote",
      "Agents",
      "OCR",
      "Server",
      "Account",
      "Setup",
      "About",
    ]);
    expect(GROUPS.map((g) => [g.title, g.pages.map((p) => p.id)])).toEqual([
      [null, ["general", "notifications"]],
      ["Your notes", ["supernote", "agents", "ocr"]],
      ["Connection", ["server", "account"]],
      [null, ["setup", "about"]],
    ]);
  });

  it("names the Setup page as the panel opens it", () => {
    expect(isPageId("setup")).toBe(true);
    expect(pageById("setup").title).toBe("Setup");
    expect(isPageId("nothing")).toBe(false);
    expect(isPageId(null)).toBe(false);
  });

  it("gives every page an icon and a colour of its own", () => {
    for (const page of PAGES) {
      expect(page.icon.length).toBeGreaterThan(0);
      expect(page.color).toMatch(/^#[0-9a-f]{6}$/i);
    }
    expect(pageById("general").color).toBe("#8e8e93");
    expect(pageById("about").color).toBe("#8e8e93");
    expect(pageById("agents").color).toBe("#30b0c7");
  });
});
