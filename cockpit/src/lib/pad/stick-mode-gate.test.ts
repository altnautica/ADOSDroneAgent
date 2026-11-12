import { describe, expect, it } from "vitest";

import { stickModeTap } from "@/lib/pad/stick-mode-gate";

describe("switching the stick mode", () => {
  it("is refused unless the vehicle is known grounded and disarmed", () => {
    expect(stickModeTap("mode2", "mode1", null, false)).toEqual({ kind: "refused" });
    expect(stickModeTap("mode2", "mode1", "mode1", false)).toEqual({ kind: "refused" });
  });

  it("takes a second tap on the same choice", () => {
    expect(stickModeTap("mode2", "mode1", null, true)).toEqual({ kind: "confirm", target: "mode1" });
    expect(stickModeTap("mode2", "mode1", "mode1", true)).toEqual({ kind: "switch", target: "mode1" });
  });

  it("does nothing on the current mode", () => {
    expect(stickModeTap("mode2", "mode2", null, true)).toEqual({ kind: "none" });
  });
});
