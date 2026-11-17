import { describe, expect, it } from "vitest";
import { defaultBackupDir } from "../settings/api";

describe("defaultBackupDir", () => {
  it("names the folder after the local date", () => {
    expect(defaultBackupDir(new Date(2026, 10, 17, 12, 0))).toBe("~/supernote-backup-2026-11-17");
    expect(defaultBackupDir(new Date(2027, 0, 5, 0, 30))).toBe("~/supernote-backup-2027-01-05");
  });

  it("keeps the day of the clock on the wall late in the evening", () => {
    expect(defaultBackupDir(new Date(2026, 11, 31, 23, 59))).toBe("~/supernote-backup-2026-12-31");
  });
});
