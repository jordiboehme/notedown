import { describe, expect, it, vi } from "vitest";
import { render } from "svelte/server";

const calls: string[] = [];
const file = (model: string, page: string, licensor: string) => ({ model, page, licence: "Apache-2.0", licensor });
const QWEN = file("Qwen3.5-4B", "https://huggingface.co/Qwen/Qwen3.5-4B", "Qwen team");
/** The four files as `model pull --json --licences-only` names them: the Qwen model and its mmproj are one model. */
const FILES = [
  QWEN,
  QWEN,
  file("PP-DocLayoutV3", "https://huggingface.co/PaddlePaddle/PP-DocLayoutV3", "PaddlePaddle Authors"),
  file("PP-OCRv6", "https://huggingface.co/PaddlePaddle/PP-OCRv6_small_det", "PaddlePaddle Authors"),
];
const LINE =
  "Licences: Qwen3.5-4B (Qwen team): Apache-2.0; PP-DocLayoutV3 and PP-OCRv6 (PaddlePaddle Authors): Apache-2.0";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn((command: string) => {
    calls.push(command);
    if (command === "model_licences") return Promise.resolve(FILES);
    return Promise.resolve(null);
  }),
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(() => Promise.resolve(() => {})) }));

const { MODEL_TEXT, canDownload, cleanOldButton, cleanOldQuestion, filesNote, licenceGroups, licenceLine } = await import(
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
    expect(canDownload({ files: null, error: null })).toBe(false);
    expect(canDownload({ files: null, error: "The licences are not known: gone" })).toBe(false);
    expect(canDownload({ files: [], error: null })).toBe(false);
    expect(canDownload({ files: FILES, error: null })).toBe(true);
  });

  it("the files row names all four files", () => {
    expect(filesNote(["Qwen3.5-4B-Q4_K_M.gguf", "mmproj-F16.gguf", "pp-doclayoutv3.safetensors", "pp-ocrv6-small-det.onnx"])).toBe(
      "Qwen3.5-4B-Q4_K_M.gguf, mmproj-F16.gguf, pp-doclayoutv3.safetensors, pp-ocrv6-small-det.onnx",
    );
  });

  it("group the files by licence, each model once", () => {
    expect(licenceGroups(FILES)).toEqual([
      { models: [{ name: "Qwen3.5-4B", page: "https://huggingface.co/Qwen/Qwen3.5-4B" }], licensor: "Qwen team", licence: "Apache-2.0" },
      {
        models: [
          { name: "PP-DocLayoutV3", page: "https://huggingface.co/PaddlePaddle/PP-DocLayoutV3" },
          { name: "PP-OCRv6", page: "https://huggingface.co/PaddlePaddle/PP-OCRv6_small_det" },
        ],
        licensor: "PaddlePaddle Authors",
        licence: "Apache-2.0",
      },
    ]);
    expect(licenceLine(FILES)).toBe(LINE);
    expect(licenceLine([])).toBe(null);
  });

  it("link only pages on Hugging Face", () => {
    const groups = licenceGroups([file("Other", "https://example.org/x", "Someone"), ...FILES]);
    expect(groups[0].models).toEqual([{ name: "Other", page: null }]);
  });

  it("download row shows the licences before the download", async () => {
    await loadLicences();
    expect(licences.files).toEqual(FILES);
    const { body } = render(ModelDownload, { props: {} });
    const paragraph = body.match(/<p class="licences[^"]*"[^>]*>([\s\S]*?)<\/p>/)?.[1] ?? "";
    // The text of the line, without its tags and Svelte's comments.
    const text = paragraph.replace(/<!--[\s\S]*?-->/g, "").replace(/<[^>]+>/g, "");
    expect(text).toBe(LINE);
    // Each model name is a button that opens its page in the browser; no link the web view
    // could load itself.
    const names = [...paragraph.matchAll(/<button[^>]*type="button"[^>]*class="link[^"]*"[^>]*>([^<]+)<\/button>/g)].map((m) => m[1]);
    expect(names).toEqual(["Qwen3.5-4B", "PP-DocLayoutV3", "PP-OCRv6"]);
    expect(paragraph).not.toContain("href");
    expect(body).toContain(MODEL_TEXT.note);
    expect(body).toContain(MODEL_TEXT.size);
    // Under the model row, above the status row with the Download button.
    expect(body.indexOf("Licences:")).toBeGreaterThan(body.indexOf(MODEL_TEXT.size));
    expect(body.indexOf("Licences:")).toBeLessThan(body.indexOf("Status"));
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
