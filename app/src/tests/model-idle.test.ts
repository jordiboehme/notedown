import { describe, expect, it } from "vitest";
import { MODEL_IDLE, modelIdleLabel, modelIdleMinutes, modelIdleOptions, modelIdleValue } from "../settings/modelIdle";

describe("the model idle row of the OCR page", () => {
  it("has a title and a note", () => {
    expect(MODEL_IDLE.title).toBe("Free memory after");
    expect(MODEL_IDLE.note).toBe(
      "Notedown unloads the OCR model after this long without new pages. It loads again for the next page.",
    );
  });

  it("offers 1, 5, 15 and 60 minutes", () => {
    expect(modelIdleOptions()).toEqual([
      { value: "1", label: "1 min" },
      { value: "5", label: "5 min" },
      { value: "15", label: "15 min" },
      { value: "60", label: "1 hour" },
    ]);
  });

  it("has a label for each value", () => {
    expect(modelIdleLabel(1)).toBe("1 min");
    expect(modelIdleLabel(5)).toBe("5 min");
    expect(modelIdleLabel(15)).toBe("15 min");
    expect(modelIdleLabel(60)).toBe("1 hour");
  });

  it("turns the minutes into the control's value and back", () => {
    expect(modelIdleValue(15)).toBe("15");
    expect(modelIdleMinutes("60")).toBe(60);
    // Without a config yet the control shows the default.
    expect(modelIdleValue(undefined)).toBe("5");
  });
});
