import { describe, expect, it } from "vitest";

import { nonDefault, outOfRange, paramMatches } from "@/lib/params-entry";

const RTL_ALT = { humanName: "RTL Altitude", description: "The minimum altitude to return home at", range: { min: 200, max: 300000 } };

describe("the params entry rules", () => {
  it("refuses a value outside the catalog range", () => {
    expect(outOfRange(99_999_999, RTL_ALT)).toEqual({ min: 200, max: 300000 });
    expect(outOfRange(1500, RTL_ALT)).toBeNull();
    expect(outOfRange(5, undefined)).toBeNull();
  });

  it("matches the readable name and description, not only the id", () => {
    expect(paramMatches("RTL_ALT", RTL_ALT, "return home")).toBe(true);
    expect(paramMatches("RTL_ALT", RTL_ALT, "altitude")).toBe(true);
    expect(paramMatches("RTL_ALT", RTL_ALT, "rtl_")).toBe(true);
    expect(paramMatches("RTL_ALT", RTL_ALT, "battery")).toBe(false);
  });

  it("marks a value that differs from the catalog default", () => {
    expect(nonDefault(1500, { defaultValue: 1500 })).toBeNull();
    expect(nonDefault(2000, { defaultValue: 1500 })).toBe(1500);
    expect(nonDefault(0.1 + 0.2, { defaultValue: 0.3 })).toBeNull();
    expect(nonDefault(7, undefined)).toBeNull();
  });
});
