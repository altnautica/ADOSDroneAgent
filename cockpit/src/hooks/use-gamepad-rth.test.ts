import { describe, expect, it } from "vitest";

import { rthModeActive } from "@/hooks/use-gamepad";

describe("the return-home mode the pad reads", () => {
  it("counts ArduPilot and PX4 return modes, any case", () => {
    for (const m of ["RTL", "smart_rtl", "QRTL", "AUTO_RTL", "AUTO.RTL", "auto.rtl"]) expect(rthModeActive(m)).toBe(true);
  });

  it("ignores landing and other modes", () => {
    for (const m of ["LAND", "AUTO.LAND", "LOITER", "AUTO.LOITER", null, undefined]) expect(rthModeActive(m)).toBe(false);
  });
});
