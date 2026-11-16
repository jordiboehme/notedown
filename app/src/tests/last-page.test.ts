import { describe, expect, it } from "vitest";
import { LAST_PAGE_KEY, createLastPage, firstPage, type PageStorage } from "../lib/lastPage";

function memory(start: Record<string, string> = {}): PageStorage & { data: Record<string, string> } {
  const data = { ...start };
  return {
    data,
    getItem: (key) => data[key] ?? null,
    setItem: (key, value) => {
      data[key] = value;
    },
  };
}

describe("the last settings page", () => {
  it("the_last_page_is_remembered", () => {
    const storage = memory();
    const last = createLastPage(storage);
    expect(last.load()).toBe("general");
    last.save("ocr");
    expect(storage.data[LAST_PAGE_KEY]).toBe("ocr");
    expect(createLastPage(storage).load()).toBe("ocr");
  });

  it("an unknown or unreadable page is General", () => {
    expect(createLastPage(memory({ [LAST_PAGE_KEY]: "gone" })).load()).toBe("general");
    const broken: PageStorage = {
      getItem: () => {
        throw new Error("no storage");
      },
      setItem: () => {
        throw new Error("no storage");
      },
    };
    const last = createLastPage(broken);
    expect(last.load()).toBe("general");
    expect(() => last.save("about")).not.toThrow();
  });

  it("a page Rust asks for wins over the last one", () => {
    expect(firstPage("setup", "ocr")).toBe("setup");
    expect(firstPage(null, "ocr")).toBe("ocr");
    expect(firstPage("nothing", "ocr")).toBe("ocr");
  });
});
