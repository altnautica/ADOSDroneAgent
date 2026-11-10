import { describe, expect, it } from "vitest";

import { FC_FAILSAFE_MODES, FC_LAND_MODES, FC_RETURN_MODES, isFcFailsafeMode } from "./fc-failsafe-modes";

describe("flight-controller return and landing modes", () => {
  it("covers the ArduPilot and PX4 names", () => {
    for (const m of ["RTL", "SMART_RTL", "QRTL", "AUTO_RTL", "AUTO.RTL"]) expect(FC_RETURN_MODES).toContain(m);
    for (const m of ["LAND", "QLAND", "LOITER_ALT_QLAND", "AUTO.LAND", "AUTO.PRECLAND"]) expect(FC_LAND_MODES).toContain(m);
    expect(Object.keys(FC_FAILSAFE_MODES).sort()).toEqual([...FC_RETURN_MODES, ...FC_LAND_MODES].sort());
  });

  it("matches any case and rejects other modes", () => {
    expect(isFcFailsafeMode("auto.rtl")).toBe(true);
    expect(isFcFailsafeMode("AUTO.PRECLAND")).toBe(true);
    expect(isFcFailsafeMode("AUTO.LOITER")).toBe(false);
    expect(isFcFailsafeMode("LOITER")).toBe(false);
    expect(isFcFailsafeMode(null)).toBe(false);
  });
});
