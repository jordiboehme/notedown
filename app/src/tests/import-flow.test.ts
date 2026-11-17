import { describe, expect, it } from "vitest";
import {
  appDataLines,
  backupProgressText,
  backupResumable,
  backupTooOld,
  firstSyncView,
  type DryRun,
  type FlowInput,
  importFailure,
  importPanel,
  importProgressText,
  importStage,
  importStatusText,
  importAgainOffered,
  newBackupDir,
  reviewActions,
  reviewLines,
  snbakRefusal,
  tooOldLine,
} from "../settings/importFlow";

// The times are local wall clock times, so "today at 14:05" holds in every time zone.
const NOW = new Date(2026, 10, 17, 16, 30).getTime();
const HOUR = 3_600_000;
const BACKUP = new Date(2026, 10, 17, 14, 5);

function iso(date: Date): string {
  return date.toISOString();
}

function dryRun(change: Partial<DryRun> = {}): DryRun {
  return {
    folders: [
      { source: "Note", target: "NOTE/Note", files: 120, folders: 4, bytes: 340_200_000 },
      { source: "Document", target: "DOCUMENT/Document", files: 3, folders: 0, bytes: 1_250_000 },
    ],
    skipped: [{ path: "REMOVABLE0", reason: "the SD card" }],
    errors: [],
    filesPresent: 0,
    files: 123,
    bytes: 341_450_000,
    needsReplace: false,
    ...change,
  };
}

/** A local setup with a complete backup made this run, nothing imported yet. */
function flow(change: Partial<FlowInput> = {}): FlowInput {
  return {
    mode: "local",
    account: { imported: false, appDataImported: false },
    job: null,
    nowMs: NOW,
    lastSyncMs: null,
    backup: { state: { state: "complete", time: iso(BACKUP) }, madeThisRun: true },
    dryRun: null,
    snbak: null,
    appData: null,
    last: null,
    again: false,
    filesThisRun: false,
    appDataThisRun: false,
    ...change,
  };
}

describe("importStage", () => {
  it("starts at the address without a backup folder", () => {
    expect(importStage(flow({ backup: null }))).toBe("address");
    expect(importStage(flow({ backup: { state: { state: "empty" }, madeThisRun: false } }))).toBe("address");
    expect(importStage(flow({ backup: { state: { state: "foreign" }, madeThisRun: false } }))).toBe("address");
  });

  it("follows the running step", () => {
    expect(importStage(flow({ job: "backup" }))).toBe("backingUp");
    expect(importStage(flow({ job: "importDryRun" }))).toBe("review");
    expect(importStage(flow({ job: "import" }))).toBe("importing");
    expect(importStage(flow({ job: "appDataDryRun" }))).toBe("appDataReview");
    expect(importStage(flow({ job: "appData" }))).toBe("appDataImporting");
  });

  it("a paused backup offers resume", () => {
    const backup = { state: { state: "resumable" as const, time: iso(BACKUP) }, madeThisRun: true };
    expect(importStage(flow({ backup }))).toBe("paused");
    // Only within the 24 hours: an older one is never continued.
    expect(importStage(flow({ backup, nowMs: BACKUP.getTime() + 25 * HOUR }))).toBe("tooOld");
  });

  it("a fresh complete backup goes to the files review", () => {
    expect(importStage(flow())).toBe("review");
    expect(importStage(flow({ dryRun: dryRun() }))).toBe("review");
  });

  it("a backup with errors shows its report", () => {
    const last = { kind: "backup" as const, ok: false, json: { errors: [{ path: "Note/a.note", message: "x" }] }, text: "1 file failed" };
    expect(importStage(flow({ last }))).toBe("backupDone");
    // A run with errors leaves a complete folder: Resume is refused there, only a new backup.
    expect(backupResumable({ state: "complete", time: iso(BACKUP) })).toBe(false);
    expect(backupResumable({ state: "resumable", time: iso(BACKUP) })).toBe(true);
    // Resume is offered only within the 24 hours, and not over files the Supernote dropped.
    expect(importStage(flow({ last, nowMs: BACKUP.getTime() + 25 * HOUR }))).toBe("tooOld");
    const stale = { ...last, json: { errors: [], stale: ["Note/gone.note"] } };
    expect(importStage(flow({ last: stale }))).toBe("tooOld");
    // A backup that could not start leaves no report: back to the address with its text.
    const refused = { kind: "backup" as const, ok: false, json: null, text: "no connection" };
    const empty = { state: { state: "empty" as const }, madeThisRun: false };
    expect(importStage(flow({ last: refused, backup: empty }))).toBe("address");
  });

  it("a backup older than 24 hours goes to tooOld", () => {
    expect(backupTooOld(BACKUP.getTime(), BACKUP.getTime() + 24 * HOUR, null)).toBe(false);
    expect(backupTooOld(BACKUP.getTime(), BACKUP.getTime() + 24 * HOUR + 1, null)).toBe(true);
    expect(importStage(flow({ nowMs: BACKUP.getTime() + 30 * HOUR }))).toBe("tooOld");
    expect(tooOldLine(BACKUP.getTime(), BACKUP.getTime() + 30 * HOUR, null)).toBe(
      "This backup is older than 24 hours. Make a new backup into a new folder.",
    );
  });

  it("a backup older than the last sync goes to tooOld", () => {
    const lastSyncMs = BACKUP.getTime() + HOUR;
    expect(backupTooOld(BACKUP.getTime(), NOW, lastSyncMs)).toBe(true);
    expect(backupTooOld(BACKUP.getTime(), NOW, BACKUP.getTime() - HOUR)).toBe(false);
    expect(importStage(flow({ lastSyncMs }))).toBe("tooOld");
    expect(tooOldLine(BACKUP.getTime(), NOW, lastSyncMs)).toBe(
      "The Supernote synced after this backup. Make a new backup into a new folder.",
    );
    // A backup.json time that cannot be read is too old as well.
    expect(backupTooOld(Number.NaN, NOW, null)).toBe(true);
  });

  it("a failed import shows its text and offers to try again with replace", () => {
    const last = { kind: "import" as const, ok: false, json: null, text: "the import was stopped" };
    expect(importStage(flow({ last, dryRun: dryRun() }))).toBe("failed");
    expect(importFailure(last)).toEqual({
      text: "The import did not finish: the import was stopped",
      replace: true,
    });
    // A result line with an error says that error.
    const done = { kind: "import" as const, ok: false, json: { error: "the disk is full" }, text: null };
    expect(importFailure(done).text).toBe("The import did not finish: the disk is full");
  });

  it("the app data review comes after the files import", () => {
    const account = { imported: true, appDataImported: false };
    expect(importStage(flow({ account }))).toBe("snbak");
    const snbak = { timeMs: NOW - HOUR, chosenThisRun: true };
    const appData = { dryRun: true, digests: 4, groups: 1, marks: 2, leftToDevice: [], withoutGroup: [], skipped: [], replaced: 0 };
    expect(importStage(flow({ account, snbak, appData }))).toBe("appDataReview");
    expect(importStage(flow({ account: { imported: true, appDataImported: true } }))).toBe("done");
    const last = { kind: "appData" as const, ok: false, json: null, text: "x" };
    expect(importStage(flow({ account, snbak, appData, last }))).toBe("appDataFailed");
  });

  it("a bad snbak after the files import goes back to the snbak choice, not to the files", () => {
    const account = { imported: true, appDataImported: false };
    const snbak = { timeMs: NOW - HOUR, chosenThisRun: true };
    const last = { kind: "appDataDryRun" as const, ok: false, json: null, text: "the .snbak holds no digests" };
    // The files review of this run is still known, but the files are not imported again.
    expect(importStage(flow({ account, snbak, last, dryRun: dryRun() }))).toBe("snbak");
  });

  it("an snbak older than 24 hours is refused", () => {
    expect(snbakRefusal(NOW - 23 * HOUR, NOW)).toBeNull();
    expect(snbakRefusal(NOW - 25 * HOUR, NOW)).toBe(
      "This .snbak file is older than 24 hours. Make a new one on the Supernote and copy it to this Mac.",
    );
    const account = { imported: true, appDataImported: false };
    const appData = { dryRun: true, digests: 4, groups: 1, marks: 2, leftToDevice: [], withoutGroup: [], skipped: [], replaced: 0 };
    const snbak = { timeMs: NOW - 25 * HOUR, chosenThisRun: true };
    expect(importStage(flow({ account, snbak, appData }))).toBe("snbak");
  });

  it("import again never offers the stored backup", () => {
    const account = { imported: true, appDataImported: true };
    const lastSyncMs = NOW - 2 * HOUR;
    expect(importAgainOffered("local", account, lastSyncMs)).toBe(true);
    expect(importAgainOffered("local", account, null)).toBe(false);
    expect(importAgainOffered("remote", account, lastSyncMs)).toBe(false);
    // The backup folder of the record is complete and fresh, but it was not made in this run.
    const stored = { state: { state: "complete" as const, time: iso(new Date(NOW - HOUR)) }, madeThisRun: false };
    expect(importStage(flow({ again: true, account, lastSyncMs: null, backup: stored }))).toBe("address");
    // A new backup of this run goes on to the review, though the account is imported.
    const made = { state: { state: "complete" as const, time: iso(new Date(NOW - HOUR)) }, madeThisRun: true };
    expect(importStage(flow({ again: true, account, lastSyncMs, backup: made }))).toBe("review");
    // The .snbak of the record is not used either.
    const snbak = { timeMs: NOW - HOUR, chosenThisRun: false };
    expect(importStage(flow({ again: true, account, lastSyncMs, backup: made, snbak, filesThisRun: true }))).toBe(
      "snbak",
    );
    expect(
      importStage(flow({ again: true, account, lastSyncMs, backup: made, filesThisRun: true, appDataThisRun: true })),
    ).toBe("done");
  });

  it("an import of this run counts while the server starts again", () => {
    // The server is stopped: no import state from it, but the files import ended well.
    const last = { kind: "import" as const, ok: true, json: { files: 123 }, text: null };
    expect(importStage(flow({ account: null, last, filesThisRun: true }))).toBe("snbak");
    const appData = { kind: "appData" as const, ok: true, json: { digests: 4 }, text: null };
    expect(importStage(flow({ account: null, last: appData, filesThisRun: true, appDataThisRun: true }))).toBe("done");
  });

  it("another machine imports there", () => {
    expect(importStage(flow({ mode: "remote", account: { imported: false, appDataImported: null } }))).toBe("address");
    expect(importStage(flow({ mode: "remote", account: { imported: true, appDataImported: null } }))).toBe("done");
  });
});

describe("the review", () => {
  it("review lines list folders, skips, the backup time and the replace note", () => {
    const lines = reviewLines(
      dryRun({ errors: ["Note/x.note: a dot name"], filesPresent: 7, needsReplace: true }),
      BACKUP.getTime(),
      NOW,
    );
    expect(lines).toEqual([
      "Backup made today at 14:05.",
      "Note: 120 files, 340.2 MB",
      "Document: 3 files, 1.3 MB",
      "Skipped REMOVABLE0 (the SD card).",
      "Error: Note/x.note: a dot name",
      "The server already holds 7 files. Replace moves them to the recycle bin first.",
    ]);
    const yesterday = reviewLines(dryRun(), new Date(2026, 10, 16, 21, 40).getTime(), NOW);
    expect(yesterday[0]).toBe("Backup made yesterday at 21:40.");
  });

  it("an empty supernote offers the empty choice", () => {
    const empty = dryRun({ folders: [], skipped: [], files: 0, bytes: 0 });
    expect(reviewLines(empty, BACKUP.getTime(), NOW)).toContain("The Supernote is empty.");
    expect(reviewActions(empty)).toEqual({ canImport: true, replace: false, empty: true });
    expect(reviewActions(dryRun())).toEqual({ canImport: true, replace: false, empty: false });
    expect(reviewActions(dryRun({ needsReplace: true, filesPresent: 3 }))).toEqual({
      canImport: true,
      replace: true,
      empty: false,
    });
    expect(reviewActions(dryRun({ errors: ["x"] })).canImport).toBe(false);
  });

  it("app data lines count digests and what stays on the Supernote", () => {
    expect(
      appDataLines({
        dryRun: true,
        digests: 4,
        groups: 1,
        marks: 2,
        leftToDevice: ["a"],
        withoutGroup: [],
        skipped: ["b", "c"],
        replaced: 3,
      }),
    ).toEqual([
      "4 digests, 1 group and 2 handwritten comments.",
      "1 record stays on the Supernote.",
      "2 entries are skipped: no imported digest names them.",
      "Replace clears 3 digest records on the server first.",
    ]);
    expect(
      appDataLines({ dryRun: true, digests: 0, groups: 0, marks: 0, leftToDevice: [], withoutGroup: [], skipped: [], replaced: 0 }),
    ).toEqual(["No digests."]);
  });
});

describe("progress and texts", () => {
  it("counts the backup", () => {
    expect(backupProgressText(42, 12_345_678)).toBe("42 files, 12.3 MB so far");
    expect(backupProgressText(1, 0)).toBe("1 file, 0.0 MB so far");
  });

  it("names the phase of the import", () => {
    const p = { kind: "import" as const, files: null, bytes: null, done: null, total: null, phase: null };
    expect(importProgressText({ ...p, phase: "recycling", files: 30 })).toBe("Moving 30 files to the recycle bin.");
    expect(importProgressText({ ...p, phase: "importing", done: 100, total: 340 })).toBe("Imported 100 of 340 files.");
    expect(importProgressText({ ...p, phase: "verifying" })).toBe("Checking every file against the backup.");
    expect(importProgressText({ ...p, phase: "restarting" })).toBe("Starting the server again.");
    expect(importProgressText({ ...p, phase: null })).toBeNull();
  });

  it("import status texts", () => {
    expect(importStatusText({ imported: true, appDataImported: true })).toBe("Imported");
    expect(importStatusText({ imported: true, appDataImported: false })).toBe("Files imported, digests not yet");
    expect(importStatusText({ imported: false, appDataImported: false })).toBe("Not imported yet");
    expect(importStatusText({ imported: null, appDataImported: null })).toBe("Not known. Update the server.");
    expect(importStatusText({ imported: true, appDataImported: null })).toBe("Not known. Update the server.");
    expect(importStatusText(null)).toBe("Not known yet.");
  });

  it("a new backup goes into a new folder", () => {
    const today = new Date(2026, 10, 17, 9, 0);
    expect(newBackupDir(today, 0)).toBe("~/supernote-backup-2026-11-17");
    expect(newBackupDir(today, 1)).toBe("~/supernote-backup-2026-11-17-2");
    expect(newBackupDir(today, 2)).toBe("~/supernote-backup-2026-11-17-3");
  });
});

describe("importPanel", () => {
  it("stays shown with the last result while the server starts again", () => {
    const last = { kind: "import" as const, ok: true, json: { files: 3 }, text: null };
    // Blocked for a moment after the import: the part stays, and asks for Start when the
    // server does not run.
    expect(importPanel({ state: "blocked", job: null, last, mode: "local", server: "stopped" })).toEqual({
      show: true,
      line: "Press Start in step 1.",
    });
    expect(importPanel({ state: "blocked", job: null, last, mode: "local", server: "running" })).toEqual({
      show: true,
      line: null,
    });
    // While it starts again, step 1 offers no Start: no line.
    expect(importPanel({ state: "blocked", job: null, last, mode: "local", server: "starting" }).line).toBeNull();
    expect(importPanel({ state: "blocked", job: null, last, mode: "local", server: "backoff" }).line).toBe(
      "Press Start in step 1.",
    );
    // Blocked before anything ran: only the step's own line.
    expect(importPanel({ state: "blocked", job: null, last: null, mode: "local", server: "stopped" }).show).toBe(
      false,
    );
    // A running step is always shown.
    expect(importPanel({ state: "blocked", job: "import", last: null, mode: "local", server: "stopped" }).show).toBe(
      true,
    );
    expect(importPanel({ state: "todo", job: null, last: null, mode: "local", server: "running" })).toEqual({
      show: true,
      line: null,
    });
  });
});

describe("firstSyncView", () => {
  it("says nothing before the check", () => {
    expect(firstSyncView(null, false, "local")).toEqual({ line: null, banner: null });
  });

  it("a clean first sync turns Auto Sync on", () => {
    const clean = { files: 0, folders: 0, digests: 0, cannotTell: false };
    expect(firstSyncView(clean, false, "local")).toEqual({
      line: "The first sync worked. Nothing was deleted. Now turn Auto Sync on, on the Supernote.",
      banner: null,
    });
    // Another machine: the digests are not checked, and the step says so.
    expect(firstSyncView({ ...clean, digests: null }, false, "remote")).toEqual({
      line: "The first sync worked. Nothing was deleted. Now turn Auto Sync on, on the Supernote. Deleted digests are not checked for a server on another machine.",
      banner: null,
    });
  });

  it("deletes show a banner until they are seen", () => {
    const deleted = { files: 3, folders: 1, digests: 2, cannotTell: false };
    expect(firstSyncView(deleted, false, "local")).toEqual({
      line: null,
      banner: [
        "The Supernote deleted 3 files on the server. They are in the recycle bin.",
        "It also deleted 1 folder and 2 digests.",
      ],
    });
    expect(firstSyncView({ ...deleted, folders: 0, digests: null }, false, "remote").banner).toEqual([
      "The Supernote deleted 3 files on the server. They are in the recycle bin.",
      "Deleted digests are not checked for a server on another machine.",
    ]);
    // Only folders: the banner leads with them.
    expect(firstSyncView({ files: 0, folders: 2, digests: 0, cannotTell: false }, false, "local").banner).toEqual([
      "The Supernote deleted 2 folders on the server. They are in the recycle bin.",
    ]);
    // Seen: the step is done, Auto Sync next.
    expect(firstSyncView(deleted, true, "local")).toEqual({
      line: "Now turn Auto Sync on, on the Supernote.",
      banner: null,
    });
  });

  it("a check that cannot tell asks to look at the recycle bin", () => {
    const unknown = { files: 0, folders: 0, digests: null, cannotTell: true };
    expect(firstSyncView(unknown, false, "remote").banner).toEqual([
      "Notedown cannot tell what the first sync changed. Check the recycle bin.",
    ]);
  });
});
