// The "Free memory after" row of the OCR page: how long the pipeline keeps the OCR model
// loaded without new pages (config.rs MODEL_IDLE_MINUTES, the set_model_idle command).

export const MODEL_IDLE = {
  title: "Free memory after",
  note: "Notedown unloads the OCR model after this long without new pages. It loads again for the next page.",
};

/** The minutes the row offers, as the app config keeps them. */
export const MODEL_IDLE_MINUTES = [1, 5, 15, 60] as const;

/** The minutes of a config that is not read yet (the default of config.rs). */
const DEFAULT_MINUTES = 5;

export function modelIdleLabel(minutes: number): string {
  return minutes === 60 ? "1 hour" : `${minutes} min`;
}

/** The options of the Segmented control, whose values are strings. */
export function modelIdleOptions(): { value: string; label: string }[] {
  return MODEL_IDLE_MINUTES.map((minutes) => ({ value: String(minutes), label: modelIdleLabel(minutes) }));
}

export function modelIdleValue(minutes: number | undefined): string {
  return String(minutes ?? DEFAULT_MINUTES);
}

export function modelIdleMinutes(value: string): number {
  return Number(value);
}
