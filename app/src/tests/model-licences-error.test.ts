import { describe, expect, it, vi } from "vitest";
import { render } from "svelte/server";

let fail = true;
const calls: string[] = [];

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn((command: string) => {
    calls.push(command);
    if (command === "model_licences") {
      return fail ? Promise.reject("The licences are not known: it did not answer in time") : Promise.resolve([{ model: "A", page: "https://huggingface.co/a/A", licence: "B", licensor: "C" }]);
    }
    return Promise.resolve(null);
  }),
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(() => Promise.resolve(() => {})) }));

const { licences, loadLicences } = await import("../settings/modelLicences.svelte");
const ModelDownload = (await import("../settings/parts/ModelDownload.svelte")).default;

describe("licences that cannot be read", () => {
  it("show the error with a retry", async () => {
    await loadLicences();
    expect(licences.files).toBe(null);
    const { body } = render(ModelDownload, { props: {} });
    expect(body).toContain("The licences are not known: it did not answer in time");
    expect(body).toContain("Retry");
  });

  it("are asked for again on retry", async () => {
    fail = false;
    await loadLicences();
    expect(licences.files).toEqual([{ model: "A", page: "https://huggingface.co/a/A", licence: "B", licensor: "C" }]);
    expect(licences.error).toBe(null);
    expect(calls.filter((c) => c === "model_licences")).toHaveLength(2);
    expect(calls).not.toContain("model_pull");
  });
});
