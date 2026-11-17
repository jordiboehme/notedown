import { describe, expect, it } from "vitest";
import type { AppConfig, NotificationChoices } from "../settings/api";
import { DOT_LINE, NOTIFICATIONS_PANE, notificationRows, withChoice } from "../settings/notifications";

const choices = (enabled: boolean): NotificationChoices => ({
  enabled,
  sync: true,
  ocr: true,
  agent: false,
  problems: true,
});

const config = (enabled: boolean) => ({ notifications: choices(enabled) }) as AppConfig;

describe("the Notifications page", () => {
  it("has the switch and one row per event, each with a note", () => {
    const view = notificationRows(config(true), "allowed");
    expect(view.master.title).toBe("Show notifications");
    expect(view.master.checked).toBe(true);
    expect(view.master.disabled).toBe(false);
    expect(view.events.map((row) => [row.key, row.title, row.checked, row.disabled])).toEqual([
      ["sync", "The Supernote synced", true, false],
      ["ocr", "OCR is done", true, false],
      ["agent", "An agent read new notes", false, false],
      ["problems", "Something needs attention", true, false],
    ]);
    for (const row of view.events) expect(row.note.length).toBeGreaterThan(0);
    expect(view.events[0].note).toBe("How many files changed.");
    expect(view.events[1].note).toBe("How many notes are ready to read.");
    expect(view.events[2].note).toBe("Which agent read them.");
    expect(view.events[3].note).toBe(
      "The server does not answer, the OCR stopped or the Supernote has not synced for 7 days.",
    );
  });

  it("turns the event rows off while the switch is off", () => {
    const view = notificationRows(config(false), "allowed");
    expect(view.master.checked).toBe(false);
    expect(view.master.disabled).toBe(false);
    for (const row of view.events) expect(row.disabled).toBe(true);
    // The choices stay as saved.
    expect(view.events.map((row) => row.checked)).toEqual([true, true, false, true]);
  });

  it("turns everything off until the config is read", () => {
    const view = notificationRows(null, null);
    expect(view.master.checked).toBe(false);
    expect(view.master.disabled).toBe(true);
    for (const row of view.events) expect(row.disabled).toBe(true);
    expect(view.permission).toBeNull();
  });

  it("says what macOS allows", () => {
    expect(notificationRows(config(true), "allowed").permission).toEqual({
      text: "Notedown may show notifications.",
      tone: "secondary",
      openSettings: false,
    });
    expect(notificationRows(config(true), "denied").permission).toEqual({
      text: "Turned off in System Settings. Open System Settings to allow Notedown.",
      tone: "bad",
      openSettings: true,
    });
    expect(notificationRows(config(false), "unknown").permission).toEqual({
      text: "Notedown asks when you turn this on.",
      tone: "secondary",
      openSettings: false,
    });
    expect(notificationRows(config(false), "unavailable").permission).toEqual({
      text: "Notifications need the installed app.",
      tone: "secondary",
      openSettings: false,
    });
  });

  it("keeps the dot with notifications off", () => {
    expect(DOT_LINE).toBe("The dot on the menu bar icon shows problems also with notifications off.");
  });

  it("opens the Notifications pane of System Settings", () => {
    expect(NOTIFICATIONS_PANE).toBe("x-apple.systempreferences:com.apple.Notifications-Settings.extension");
  });

  it("changes one choice and keeps the others", () => {
    expect(withChoice(choices(false), "enabled", true)).toEqual(choices(true));
    expect(withChoice(choices(true), "agent", true)).toEqual({ ...choices(true), agent: true });
  });
});
