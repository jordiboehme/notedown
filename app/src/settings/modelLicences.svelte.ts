// The licences of the model files. The model rows show them before the download, so they are
// asked for once when the rows first show; they do not change while the app runs.

import { failure, modelLicences } from "./api";

export const licences = $state<{ lines: string[] | null; error: string | null }>({ lines: null, error: null });

let asked: Promise<void> | null = null;

/** Asks the notedown program for the licence lines, once. A failure may be asked again. */
export function loadLicences(): Promise<void> {
  asked ??= modelLicences()
    .then((lines) => {
      licences.lines = lines;
      licences.error = null;
    })
    .catch((e) => {
      licences.error = failure(e);
      asked = null;
    });
  return asked;
}
