// The updater on the About page: the state Rust tells (update.rs UpdateState) and its lines.

export type UpdateState =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "found"; version: string; notes: string | null }
  | { kind: "upToDate" }
  | { kind: "failed"; reason: string }
  | { kind: "installing"; version: string }
  | { kind: "installFailed"; version: string; reason: string };

/** A reason from Rust ends the sentence: without its own period, never empty. */
function because(reason: string): string {
  const text = reason.trim().replace(/\.+$/, "");
  return text === "" ? "no reason given" : text;
}

/** What the About page says of the updater, one line each. */
export function updateLines(state: UpdateState): string[] {
  switch (state.kind) {
    case "idle":
      return [];
    case "checking":
      return ["Checking for an update..."];
    case "found":
      return [`Notedown ${state.version} is ready. Install it now? Notedown starts again after the update.`];
    case "upToDate":
      return ["Notedown is up to date."];
    case "failed":
      return [`The update could not be checked: ${because(state.reason)}.`];
    case "installing":
      return [`Installing Notedown ${state.version}. Notedown starts again after the update.`];
    case "installFailed":
      return [`Notedown ${state.version} could not be installed: ${because(state.reason)}.`];
  }
}

/** A check may start unless one runs or an update installs. */
export function canCheck(state: UpdateState): boolean {
  return state.kind !== "checking" && state.kind !== "installing";
}

/** The install is offered for a found update, and again after a failed install. */
export function canInstall(state: UpdateState): boolean {
  return state.kind === "found" || state.kind === "installFailed";
}
