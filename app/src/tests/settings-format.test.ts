import { describe, expect, it } from "vitest";
import { formatBytes, fraction } from "../settings/format";

describe("the numbers of the settings pages", () => {
  it("says sizes as the Finder does", () => {
    expect(formatBytes(0)).toBe("0 bytes");
    expect(formatBytes(1)).toBe("1 byte");
    expect(formatBytes(999)).toBe("999 bytes");
    expect(formatBytes(1500)).toBe("1.5 KB");
    expect(formatBytes(366_894_368)).toBe("367 MB");
    expect(formatBytes(3_066_384_992 + 366_894_368)).toBe("3.4 GB");
  });

  it("keeps the share of a download between 0 and 1", () => {
    expect(fraction(50, 100)).toBe(0.5);
    expect(fraction(1, 0)).toBe(0);
    expect(fraction(200, 100)).toBe(1);
  });
});
