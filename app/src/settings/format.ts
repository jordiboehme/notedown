// Numbers for the settings pages.

import type { SyncCount } from "../lib/api";

/** Bytes in the units the Finder uses (powers of 1000): "3.4 GB", "512 MB", "0 bytes". */
export function formatBytes(bytes: number): string {
  if (bytes < 1000) return bytes === 1 ? "1 byte" : `${bytes} bytes`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes;
  let unit = -1;
  while (value >= 1000 && unit < units.length - 1) {
    value /= 1000;
    unit += 1;
  }
  const digits = value < 10 ? 1 : 0;
  return `${value.toFixed(digits)} ${units[unit]}`;
}

/** The share of a download, 0 to 1. */
export function fraction(done: number, total: number): number {
  if (total <= 0) return 0;
  return Math.min(1, Math.max(0, done / total));
}

/** The files the last device sync changed, for the Supernote page. */
export function filesChangedText(count: SyncCount | null): string {
  const files = count?.files ?? null;
  if (files === null) return "Not known";
  if (files === 0) return "No files changed";
  return files === 1 ? "1 file changed" : `${files} files changed`;
}
