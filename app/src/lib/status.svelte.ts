import { listen } from "@tauri-apps/api/event";
import { STATUS_EVENT, getStatus, type Status } from "./api";

/** The last status from Rust; null until the first one arrives. */
export const status = $state<{ current: Status | null }>({ current: null });

/**
 * Follows the `status` event and asks once for the status as it is now. An event that comes
 * before that answer is newer, so the answer is then dropped. Returns the function that stops
 * following.
 */
export async function startStatus(): Promise<() => void> {
  let heard = false;
  const unlisten = await listen<Status>(STATUS_EVENT, (event) => {
    heard = true;
    status.current = event.payload;
  });
  getStatus()
    .then((first) => {
      if (!heard) status.current = first;
    })
    .catch((error: unknown) => {
      console.error("notedown: the status could not be read", error);
    });
  return unlisten;
}
