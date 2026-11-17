import { describe, expect, it } from "vitest";
import type { ChildState } from "../lib/api";
import { markdownRow, type MarkdownFolder } from "../settings/markdown";

const base: MarkdownFolder = { path: "/Users/j/Documents/Markdown", error: null, busy: false, pipeline: null };
const running: ChildState = { state: "running", pid: 7, sinceMs: 1 };

describe("the Markdown folder row of the OCR page", () => {
  it("shows the path with Choose... and Show in Finder", () => {
    const row = markdownRow({ ...base, pipeline: running });
    expect(row.title).toBe("Markdown folder");
    expect(row.explanation).toBe("Notedown writes the Markdown of your notes here.");
    expect(row.path).toBe("/Users/j/Documents/Markdown");
    expect(row.note).toBeNull();
    expect(row.choose).toEqual({ label: "Choose...", enabled: true });
    expect(row.show).toEqual({ label: "Show in Finder", enabled: true });
  });

  it("waits for the folder before it offers anything", () => {
    const row = markdownRow({ ...base, path: undefined });
    expect(row.path).toBeNull();
    expect(row.note).toBe("Checking...");
    expect(row.choose.enabled).toBe(false);
    expect(row.show.enabled).toBe(false);
  });

  it("asks for a login first: the folder belongs to the account", () => {
    const row = markdownRow({ ...base, path: null });
    expect(row.path).toBeNull();
    expect(row.note).toBe("Log in to choose the Markdown folder.");
    expect(row.tone).toBe("secondary");
    expect(row.choose.enabled).toBe(false);
    expect(row.show.enabled).toBe(false);
  });

  it("says why a choice was refused, and holds the buttons while it saves", () => {
    const refused = markdownRow({ ...base, error: "Choose a folder outside the Notedown data folder." });
    expect(refused.note).toBe("Choose a folder outside the Notedown data folder.");
    expect(refused.tone).toBe("bad");
    expect(refused.path).toBe(base.path);
    const busy = markdownRow({ ...base, busy: true });
    expect(busy.choose.enabled).toBe(false);
    expect(busy.show.enabled).toBe(false);
  });

  it("shows the pipeline's own refusal of the folder", () => {
    const pipeline: ChildState = {
      state: "backoff",
      untilMs: 5,
      lastError:
        "error: invalid --markdown-dir: the Markdown folder /d/md is inside the server's data folder /d; pick a folder outside it",
    };
    const row = markdownRow({ ...base, pipeline });
    expect(row.note).toBe(
      "The pipeline refused this folder: the Markdown folder /d/md is inside the server's data folder /d; pick a folder outside it",
    );
    expect(row.tone).toBe("bad");
    expect(row.choose.enabled).toBe(true);
    // Another error of the pipeline is not about the folder: the Queue shows the pipeline.
    const other = markdownRow({ ...base, pipeline: { ...pipeline, lastError: "error: the OCR model is missing" } });
    expect(other.note).toBeNull();
    // Our own refusal comes first.
    expect(markdownRow({ ...base, pipeline, error: "Not saved." }).note).toBe("Not saved.");
  });
});
