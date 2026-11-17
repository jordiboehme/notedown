import { describe, expect, it, vi } from "vitest";
import { render } from "svelte/server";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(() => Promise.resolve(null)) }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(() => Promise.resolve(() => {})) }));

const SupernoteStep = (await import("../settings/setup/SupernoteStep.svelte")).default;

describe("the device steps", () => {
  it("the Supernote page leaves the Auto Sync rows out", () => {
    const { body } = render(SupernoteStep, { props: { check: false } });
    expect(body).toContain("Retain locally");
    expect(body).not.toContain("Leave Auto Sync off for now.");
    expect(body).not.toContain("Sync once by hand.");
  });

  it("step 6 says to leave Auto Sync off until it is done", () => {
    expect(render(SupernoteStep, { props: { done: false } }).body).toContain("Leave Auto Sync off for now.");
    const done = render(SupernoteStep, { props: { done: true } }).body;
    expect(done).not.toContain("Leave Auto Sync off for now.");
    expect(done).toContain("Turn Auto Sync on, on the Supernote.");
  });
});
