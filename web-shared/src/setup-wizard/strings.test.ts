import { describe, expect, it } from "vitest";

import { optionText, setupText } from "./strings";

describe("setup wizard text", () => {
  it("falls back from a body variant to the step body, and only there", () => {
    expect(setupText("setup.accel.body.no_such_phase")).toBe(setupText("setup.accel.body"));
    expect(setupText("setup.accel.title.no_such_phase")).toBe("setup.accel.title.no_such_phase");
  });

  it("labels a bare enum option by the engine's <input key>.<value> convention", () => {
    expect(optionText("setup.input.chemistry", "lipo", null)).toBe("LiPo");
    expect(optionText("setup.input.cells", 4, null)).toBe("4");
    expect(optionText("setup.input.chemistry", "lipo", "setup.input.chemistry.liion")).toBe("Li-ion");
  });
});
