import { describe, expect, it, vi } from "vitest";
import { render } from "svelte/server";

const calls: string[] = [];
const LINES = [
  "Qwen3.5-4B (Qwen team, Alibaba Cloud): Apache-2.0",
  "PP-DocLayoutV3 and PP-OCRv6 (PaddlePaddle Authors): Apache-2.0",
];

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn((command: string) => {
    calls.push(command);
    if (command === "model_licences") return Promise.resolve(LINES);
    return Promise.resolve(null);
  }),
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(() => Promise.resolve(() => {})) }));

const { MODEL_TEXT, licenceLine } = await import("../settings/modelText");
const { licences, loadLicences } = await import("../settings/modelLicences.svelte");
const ModelDownload = (await import("../settings/parts/ModelDownload.svelte")).default;

describe("the model rows", () => {
  it("name the new model and its size", () => {
    expect(MODEL_TEXT.note).toBe("Qwen3.5-4B reads your notes on your Mac. Your notes never leave it.");
    expect(MODEL_TEXT.size).toBe("About 3.6 GB");
    expect(MODEL_TEXT.cleanOld).toBe("Remove the old model files (3.4 GB)");
  });

  it("join the licence lines", () => {
    expect(licenceLine(LINES)).toBe(
      "Licences: Qwen3.5-4B (Qwen team, Alibaba Cloud): Apache-2.0; PP-DocLayoutV3 and PP-OCRv6 (PaddlePaddle Authors): Apache-2.0",
    );
    expect(licenceLine([])).toBe(null);
  });

  it("download row shows the licences before the download", async () => {
    await loadLicences();
    expect(licences.lines).toEqual(LINES);
    const { body } = render(ModelDownload, { props: {} });
    const line = licenceLine(LINES)!;
    expect(body).toContain(MODEL_TEXT.note);
    expect(body).toContain(MODEL_TEXT.size);
    expect(body).toContain(line);
    // Under the model row, above the status row with the Download button.
    expect(body.indexOf(line)).toBeGreaterThan(body.indexOf(MODEL_TEXT.size));
    expect(body.indexOf(line)).toBeLessThan(body.indexOf("Status"));
    // The licences come from `model pull --json --licences-only`: nothing was downloaded.
    expect(calls).toEqual(["model_licences"]);
    expect(body).not.toMatch(/chandra/i);
    expect(body).not.toMatch(/[–—]/);
  });

  it("asks for the licences only once", async () => {
    await loadLicences();
    expect(calls.filter((c) => c === "model_licences")).toHaveLength(1);
  });
});
