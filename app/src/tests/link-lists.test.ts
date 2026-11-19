import { describe, expect, it } from "vitest";
import commands from "../../src-tauri/src/commands.rs?raw";
import { ABOUT } from "../settings/about";
import { SETUP_LINKS } from "../settings/api";
import { NOTIFICATIONS_PANE } from "../settings/notifications";
import { licenceGroups } from "../settings/modelText";

/** The strings of the Rust constant `name` in commands.rs (an array or a single string). */
function rustLinks(name: string): string[] {
  const start = commands.indexOf(`pub const ${name}:`);
  expect(start, name).toBeGreaterThan(-1);
  // The value: after " = ", up to the ";" that ends it (the type has a ";" of its own).
  const value = commands.indexOf(" =", start);
  const end = commands.indexOf(";", value);
  return [...commands.slice(value, end).matchAll(/"([^"]+)"/g)].map((m) => m[1]);
}

describe("the links the settings open", () => {
  it("are the ones open_link allows in Rust", () => {
    expect(ABOUT.links.map((link) => link.url)).toEqual(rustLinks("ABOUT_LINKS"));
    expect(Object.values(SETUP_LINKS)).toEqual(rustLinks("SETUP_LINKS"));
    expect([NOTIFICATIONS_PANE]).toEqual(rustLinks("NOTIFICATIONS_PANE"));
  });

  it("link a model page only in the form open_link allows: https://huggingface.co/<owner>/<model>", () => {
    // Rust's model_page: a path of exactly two plain parts.
    const model = commands.slice(commands.indexOf("fn model_page("), commands.indexOf("\n}\n", commands.indexOf("fn model_page(")));
    expect(model).toContain('strip_prefix("https://huggingface.co/")');
    expect(model).toContain("parts.len() == 2");
    const linked = (page: string) => licenceGroups([{ model: "M", page, licence: "L", licensor: "O" }])[0].models[0].page;
    expect(linked("https://huggingface.co/Qwen/Qwen3.5-4B")).toBe("https://huggingface.co/Qwen/Qwen3.5-4B");
    for (const page of ["https://huggingface.co/a", "https://huggingface.co/a/b/c", "https://example.org/a/b", "https://huggingface.co/a/b?c"]) {
      expect(linked(page), page).toBeNull();
    }
  });
});
