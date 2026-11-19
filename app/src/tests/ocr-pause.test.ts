import { describe, expect, it } from "vitest";
import { pauseRow } from "../settings/ocrPause";
import { sampleStatus } from "./sample";

const now = Date.UTC(2026, 10, 16, 12, 0, 0);

describe("pauseRow", () => {
  it("shows the pause line and Resume OCR Now only while paused", () => {
    const status = sampleStatus(now);
    expect(pauseRow(status)).toEqual({ line: null, showResume: false });

    status.ocr = { ...status.ocr, line: "OCR paused until 14:32", paused: true };
    expect(pauseRow(status)).toEqual({ line: "OCR paused until 14:32", showResume: true });

    status.ocr = { ...status.ocr, line: "OCR paused" };
    expect(pauseRow(status)).toEqual({ line: "OCR paused", showResume: true });
  });

  it("shows nothing before the first status", () => {
    expect(pauseRow(null)).toEqual({ line: null, showResume: false });
  });
});
