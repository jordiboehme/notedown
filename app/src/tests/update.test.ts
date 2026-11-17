import { describe, expect, it } from "vitest";
import { render } from "svelte/server";
import AboutPage from "../settings/pages/AboutPage.svelte";
import { canCheck, canInstall, updateLines, type UpdateState } from "../settings/update";

const found: UpdateState = { kind: "found", version: "0.2.0", notes: "# Notedown 0.2.0\n\nFaster sync." };

function page(update: UpdateState): string {
  return render(AboutPage, {
    props: { version: "0.1.0", update, onOpen: () => {}, onCheck: () => {}, onInstall: () => {} },
  }).body;
}

describe("the update lines", () => {
  it("asks before it installs a found update", () => {
    expect(updateLines(found)).toEqual([
      "Notedown 0.2.0 is ready. Install it now? Notedown starts again after the update.",
    ]);
  });

  it("says when Notedown is up to date", () => {
    expect(updateLines({ kind: "upToDate" })).toEqual(["Notedown is up to date."]);
  });

  it("says why a check failed, with one period", () => {
    expect(updateLines({ kind: "failed", reason: "the server did not answer" })).toEqual([
      "The update could not be checked: the server did not answer.",
    ]);
    expect(updateLines({ kind: "failed", reason: "Could not fetch a valid release JSON from the remote." })).toEqual([
      "The update could not be checked: Could not fetch a valid release JSON from the remote.",
    ]);
    expect(updateLines({ kind: "failed", reason: "  " })).toEqual(["The update could not be checked: no reason given."]);
  });

  it("tells the check and the install", () => {
    expect(updateLines({ kind: "idle" })).toEqual([]);
    expect(updateLines({ kind: "checking" })).toEqual(["Checking for an update..."]);
    expect(updateLines({ kind: "installing", version: "0.2.0" })).toEqual([
      "Installing Notedown 0.2.0. Notedown starts again after the update.",
    ]);
    expect(updateLines({ kind: "installFailed", version: "0.2.0", reason: "the signature does not match." })).toEqual([
      "Notedown 0.2.0 could not be installed: the signature does not match.",
    ]);
  });

  it("offers a check unless one runs, and the install for a found update", () => {
    const all: UpdateState[] = [
      { kind: "idle" },
      { kind: "checking" },
      found,
      { kind: "upToDate" },
      { kind: "failed", reason: "x" },
      { kind: "installing", version: "0.2.0" },
      { kind: "installFailed", version: "0.2.0", reason: "x" },
    ];
    expect(all.filter(canCheck).map((s) => s.kind)).toEqual(["idle", "found", "upToDate", "failed", "installFailed"]);
    expect(all.filter(canInstall).map((s) => s.kind)).toEqual(["found", "installFailed"]);
    for (const state of all) {
      for (const line of updateLines(state)) expect(line).not.toMatch(/[\u2013\u2014]/);
    }
  });
});

describe("the update on the About page", () => {
  it("shows a found update with its notes and the install button", () => {
    const body = page(found);
    expect(body).toContain("Notedown 0.2.0 is ready. Install it now? Notedown starts again after the update.");
    expect(body).toContain("Faster sync.");
    expect(body).toContain(">Install<");
    expect(body).toContain("Check for Updates");
  });

  it("shows only the check button when nothing was found", () => {
    const body = page({ kind: "idle" });
    expect(body).toContain("Check for Updates");
    expect(body).not.toContain(">Install<");
    expect(body).toContain("Version 0.1.0");
  });

  it("shows the answer of a check", () => {
    expect(page({ kind: "upToDate" })).toContain("Notedown is up to date.");
  });
});
