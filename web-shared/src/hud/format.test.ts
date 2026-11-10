import { describe, expect, it } from "vitest";

import readoutCases from "./fixtures/readout-cases.json";
import { countdownS, fmtDistance, fmtSigned1 } from "./format";

const FNS: Record<string, (v: number | null) => string | number> = {
  fmtSigned1,
  fmtDistance,
  countdownS: (v) => countdownS(v ?? 0),
};

describe("readout formatters", () => {
  it("never reads -0.0", () => {
    expect([-0.01, -0.049, -0].map(fmtSigned1)).toEqual(["0.0", "0.0", "0.0"]);
    expect(fmtSigned1(Number.NaN)).toBe("—");
    expect(fmtSigned1(undefined)).toBe("—");
  });

  it("switches to kilometres at 1000 m", () => {
    expect([999.4, 1000, 12_345].map(fmtDistance)).toEqual(["999 m", "1.00 km", "12.35 km"]);
    expect(fmtDistance(undefined)).toBe("—");
  });

  it("counts whole seconds up, never below zero", () => {
    expect([2001, 2000, 1999, 1, 0, -1].map(countdownS)).toEqual([3, 2, 2, 1, 0, 0]);
  });
});

describe("readout golden cases (fixtures/readout-cases.json)", () => {
  for (const c of readoutCases) {
    it(c.name, () => {
      expect(Object.keys(FNS)).toContain(c.input.fn);
      expect(FNS[c.input.fn](c.input.v)).toBe(c.expected);
    });
  }
});
