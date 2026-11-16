import { describe, expect, it } from "vitest";
import { relativeTime } from "../lib/format";

describe("relativeTime", () => {
  const now = Date.UTC(2026, 10, 16, 12, 0, 0);

  it("says just now for the last minute", () => {
    expect(relativeTime(now, now)).toBe("just now");
    expect(relativeTime(now - 59_000, now)).toBe("just now");
  });

  it("says a time in the future is just now", () => {
    expect(relativeTime(now + 5_000, now)).toBe("just now");
  });

  it("counts minutes, hours and days", () => {
    expect(relativeTime(now - 12 * 60_000, now)).toBe("12 min ago");
    expect(relativeTime(now - 2 * 3_600_000, now)).toBe("2 h ago");
    expect(relativeTime(now - 30 * 3_600_000, now)).toBe("yesterday");
    expect(relativeTime(now - 3 * 86_400_000, now)).toBe("3 days ago");
  });
});

describe("relativeTime beyond a few days", () => {
  const now = Date.UTC(2026, 10, 16, 12, 0, 0);

  it("counts days up to six", () => {
    expect(relativeTime(now - 6 * 86_400_000, now)).toBe("6 days ago");
  });

  it("shows the date after that", () => {
    for (const days of [7, 10, 400]) {
      const ms = now - days * 86_400_000;
      const text = relativeTime(ms, now);
      expect(text).toBe(new Date(ms).toLocaleDateString());
      expect(text).not.toContain("ago");
    }
  });
});
