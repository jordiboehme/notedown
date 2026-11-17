import { describe, expect, it } from "vitest";
import type { ChildState } from "../lib/api";
import { markdownRead, markdownRow, type MarkdownFolder } from "../settings/markdown";

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

  it("says when the folder could not be read", () => {
    const row = markdownRow({ ...base, path: undefined, error: "Notedown is quitting." });
    expect(row.note).toBe("Notedown is quitting.");
    expect(row.tone).toBe("bad");
    expect(row.choose.enabled).toBe(false);
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

  it("shows the pipeline's refusal of a folder in its own config", () => {
    const backoff = (lastError: string): ChildState => ({ state: "backoff", untilMs: 5, lastError });
    const inside = markdownRow({
      ...base,
      pipeline: backoff(
        "error: invalid markdown_dirs: the Markdown folder /d/md is inside the server's data folder /d; pick a folder outside it",
      ),
    });
    expect(inside.note).toBe(
      "The pipeline refused a Markdown folder of its own config: the Markdown folder /d/md is inside the server's data folder /d; pick a folder outside it",
    );
    expect(inside.tone).toBe("bad");
    const slug = markdownRow({
      ...base,
      pipeline: backoff('error: /d/server.toml: markdown_dirs: "a/b": not a valid account name'),
    });
    expect(slug.note).toBe(
      'The pipeline refused a Markdown folder of its own config: /d/server.toml: markdown_dirs: "a/b": not a valid account name',
    );
    expect(slug.tone).toBe("bad");
  });

  it("clears an old error when the folder reads again", () => {
    const failed = { ...base, path: undefined, error: "Notedown is quitting." };
    const read = markdownRead(failed, { path: "/Users/j/md" });
    expect(read).toEqual({ ...failed, path: "/Users/j/md", error: null });
    expect(markdownRow(read).note).toBeNull();
    // A failed read keeps the path it had and says why.
    expect(markdownRead(base, { error: "No." })).toEqual({ ...base, error: "No." });
  });
});
