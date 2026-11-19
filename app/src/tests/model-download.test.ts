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

const { MODEL_TEXT, canDownload, cleanOldButton, cleanOldQuestion, filesNote, licenceLine } = await import(
  "../settings/modelText"
);
const { licences, loadLicences } = await import("../settings/modelLicences.svelte");
const ModelDownload = (await import("../settings/parts/ModelDownload.svelte")).default;

describe("the model rows", () => {
  it("name the new model and its size", () => {
    expect(MODEL_TEXT.note).toBe("Qwen3.5-4B reads your notes on your Mac. Your notes never leave it.");
    expect(MODEL_TEXT.size).toBe("About 3.6 GB");
  });

  it("the old files button names their real size", () => {
    expect(cleanOldButton(3_433_278_000)).toBe("Remove the old model files (3.4 GB)");
    expect(cleanOldButton(12_000_000)).toBe("Remove the old model files (12 MB)");
    // Only empty leftovers: no size.
    expect(cleanOldButton(0)).toBe("Remove the old model files");
  });

  it("the old files are removed only after a question that names the size", () => {
    expect(cleanOldQuestion(3_433_278_000)).toBe(
      "Delete the old model files of Notedown 0.1? This frees 3.4 GB. The new model stays.",
    );
    expect(cleanOldQuestion(0)).toBe("Delete the old model files of Notedown 0.1? The new model stays.");
  });

  it("Download waits for the licences", () => {
    expect(canDownload({ lines: null, error: null })).toBe(false);
    expect(canDownload({ lines: null, error: "The licences are not known: gone" })).toBe(false);
    expect(canDownload({ lines: [], error: null })).toBe(false);
    expect(canDownload({ lines: LINES, error: null })).toBe(true);
  });

  it("the files row names all four files", () => {
    expect(filesNote(["Qwen3.5-4B-Q4_K_M.gguf", "mmproj-F16.gguf", "pp-doclayoutv3.safetensors", "pp-ocrv6-small-det.onnx"])).toBe(
      "Qwen3.5-4B-Q4_K_M.gguf, mmproj-F16.gguf, pp-doclayoutv3.safetensors, pp-ocrv6-small-det.onnx",
    );
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
