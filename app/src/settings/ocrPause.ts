import type { Status } from "../lib/api";

/** The pause on the OCR page: its line ("OCR paused until 14:32") and the Resume OCR Now button. */
export interface PauseRow {
  line: string | null;
  showResume: boolean;
}

/** The pause row of `status`: shown only while the OCR is paused. */
export function pauseRow(status: Status | null): PauseRow {
  if (!status?.ocr.paused) return { line: null, showResume: false };
  return { line: status.ocr.line, showResume: true };
}
