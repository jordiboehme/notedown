import { describe, expect, it } from "vitest";
import { PANEL_MAX_HEIGHT, panelHeight } from "../panel/fit";

describe("panelHeight", () => {
  it("fits the content up to the most the panel may have", () => {
    expect(PANEL_MAX_HEIGHT).toBe(520);
    expect(panelHeight(180.2)).toBe(181);
    expect(panelHeight(520)).toBe(520);
    expect(panelHeight(900)).toBe(520);
  });

  it("never goes below one line", () => {
    expect(panelHeight(0)).toBe(60);
  });
});
