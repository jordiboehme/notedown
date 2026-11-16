import { beforeEach, describe, expect, it, vi } from "vitest";
import { notSetUp, sampleStatus } from "./sample";

type Handler = (event: { payload: unknown }) => void;
const handlers = new Map<string, Handler>();
const unlisten = vi.fn();

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (name: string, handler: Handler) => {
    handlers.set(name, handler);
    return unlisten;
  }),
}));

const invoke = vi.fn(async (_command: string, _args?: unknown) => undefined);
vi.mock("@tauri-apps/api/core", () => ({ invoke: (command: string, args?: unknown) => invoke(command, args) }));

const { QUIT_QUESTION, quitQuestion, followQuitRequests, pressQuit, confirmQuit, cancelQuit } = await import(
  "../panel/quit.svelte"
);

const now = Date.UTC(2026, 10, 16, 12, 0, 0);

describe("the Quit question", () => {
  beforeEach(() => {
    handlers.clear();
    invoke.mockClear();
    quitQuestion.open = false;
  });

  it("says the words of the plan", () => {
    expect(QUIT_QUESTION).toBe("The Supernote cannot sync while Notedown is not running. Quit anyway?");
  });

  it("opens when Rust asks for it (tray Quit, Cmd+Q)", async () => {
    const stop = await followQuitRequests();
    expect(handlers.has("confirm-quit")).toBe(true);
    handlers.get("confirm-quit")!({ payload: null });
    expect(quitQuestion.open).toBe(true);
    stop();
    expect(unlisten).toHaveBeenCalled();
  });

  it("the panel's Quit asks first in Local mode with the server running", () => {
    pressQuit(sampleStatus(now));
    expect(quitQuestion.open).toBe(true);
    expect(invoke).not.toHaveBeenCalled();
  });

  it("the panel's Quit leaves the decision to Rust otherwise", () => {
    pressQuit(notSetUp());
    expect(quitQuestion.open).toBe(false);
    expect(invoke).toHaveBeenCalledWith("request_quit", undefined);

    invoke.mockClear();
    pressQuit(null);
    expect(invoke).toHaveBeenCalledWith("request_quit", undefined);
  });

  it("Quit in the question quits, Cancel only closes it", () => {
    quitQuestion.open = true;
    cancelQuit();
    expect(quitQuestion.open).toBe(false);
    expect(invoke).not.toHaveBeenCalled();

    quitQuestion.open = true;
    confirmQuit();
    expect(quitQuestion.open).toBe(false);
    expect(invoke).toHaveBeenCalledWith("quit_app", undefined);
  });
});
