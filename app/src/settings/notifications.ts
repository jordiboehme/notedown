// The Notifications page: the switch, one row per event and what macOS allows
// (config.rs NotificationChoices, the set_notifications and notification_authorization commands).
import type { AppConfig, Authorization, NotificationChoices } from "./api";

/** The Notifications pane of System Settings (commands.rs NOTIFICATIONS_PANE). */
export const NOTIFICATIONS_PANE = "x-apple.systempreferences:com.apple.Notifications-Settings.extension";

export const DOT_LINE = "The dot on the menu bar icon shows problems also with notifications off.";

export type EventKey = Exclude<keyof NotificationChoices, "enabled">;

const EVENTS: { key: EventKey; title: string; note: string }[] = [
  { key: "sync", title: "The Supernote synced", note: "How many files changed." },
  { key: "ocr", title: "OCR is done", note: "How many notes are ready to read." },
  { key: "agent", title: "An agent read new notes", note: "Which agent read them." },
  {
    key: "problems",
    title: "Something needs attention",
    note: "The server does not answer, the OCR stopped or the Supernote has not synced for 7 days.",
  },
];

const PERMISSION: Record<Authorization, { text: string; tone: "secondary" | "bad"; openSettings: boolean }> = {
  allowed: { text: "Notedown may show notifications.", tone: "secondary", openSettings: false },
  denied: {
    text: "Turned off in System Settings. Open System Settings to allow Notedown.",
    tone: "bad",
    openSettings: true,
  },
  unknown: { text: "Notedown asks when you turn this on.", tone: "secondary", openSettings: false },
  unavailable: { text: "Notifications need the installed app.", tone: "secondary", openSettings: false },
};

export interface NotificationsView {
  master: { title: string; checked: boolean; disabled: boolean };
  events: { key: EventKey; title: string; note: string; checked: boolean; disabled: boolean }[];
  /** What macOS allows; null until it is read. */
  permission: { text: string; tone: "secondary" | "bad"; openSettings: boolean } | null;
}

/** The rows of the page for the config and the permission (both null until read). */
export function notificationRows(config: AppConfig | null, authorization: Authorization | null): NotificationsView {
  const choices = config?.notifications ?? null;
  return {
    master: { title: "Show notifications", checked: choices?.enabled ?? false, disabled: choices === null },
    events: EVENTS.map((event) => ({
      ...event,
      checked: choices?.[event.key] ?? false,
      disabled: !choices?.enabled,
    })),
    permission: authorization === null ? null : PERMISSION[authorization],
  };
}

/** The choices with one of them changed. */
export function withChoice(
  choices: NotificationChoices,
  key: keyof NotificationChoices,
  on: boolean,
): NotificationChoices {
  return { ...choices, [key]: on };
}

/**
 * Reads what macOS allows. A read started while a change is busy, or overtaken by a change,
 * is old: its answer and its error are dropped (the change brings its own).
 */
export function permissionReads(busy: () => boolean) {
  let changes = 0;
  return {
    /** A change starts. */
    changed() {
      changes += 1;
    },
    read(ask: () => Promise<Authorization>, answer: (a: Authorization) => void, fail: (error: unknown) => void) {
      if (busy()) return;
      const started = changes;
      const current = () => !busy() && started === changes;
      ask().then(
        (a) => {
          if (current()) answer(a);
        },
        (error: unknown) => {
          if (current()) fail(error);
        },
      );
    },
  };
}
