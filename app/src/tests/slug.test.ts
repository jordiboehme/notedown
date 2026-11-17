import { describe, expect, it } from "vitest";
import { slugFromEmail } from "../settings/slug";

describe("slugFromEmail", () => {
  it("takes the local part in lower case with dashes", () => {
    expect(slugFromEmail("Jordi.Boehme+sn@x.de")).toBe("jordi-boehme-sn");
  });

  it("gives me when nothing is left", () => {
    expect(slugFromEmail("@x")).toBe("me");
    expect(slugFromEmail("")).toBe("me");
    expect(slugFromEmail("...@x")).toBe("me");
  });

  it("cuts to 64 characters", () => {
    expect(slugFromEmail(`${"a".repeat(80)}@x`)).toBe("a".repeat(64));
  });

  it("trims dashes and underscores from both ends", () => {
    expect(slugFromEmail("__a__@x")).toBe("a");
    expect(slugFromEmail("-a_b-@x")).toBe("a_b");
  });

  it("collapses runs of dashes and replaces letters outside a-z", () => {
    expect(slugFromEmail("a..b--c@x")).toBe("a-b-c");
    expect(slugFromEmail("Jürgen@x")).toBe("j-rgen");
  });
});
