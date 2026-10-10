import { describe, expect, it } from "vitest";

import { angleDelta, flightPath, headingTicks, isInstrumentStale, pitchRungs, tapeTicks } from "@/lib/hud-geometry";

describe("pitchRungs", () => {
  it("places a rung every 5° and labels every 10°, skipping the horizon", () => {
    const rungs = pitchRungs(0, 10, 20);
    expect(rungs.map((r) => r.deg)).toEqual([-20, -15, -10, -5, 5, 10, 15, 20]);
    expect(rungs.filter((r) => r.label).map((r) => r.deg)).toEqual([-20, -10, 10, 20]);
  });

  it("moves the ladder with pitch (nose up puts the 10° rung on the boresight)", () => {
    const ten = pitchRungs(10, 4, 15).find((r) => r.deg === 10);
    expect(ten?.y).toBe(0);
    expect(pitchRungs(10, 4, 15).find((r) => r.deg === 5)?.y).toBe(20);
  });
});

describe("tapes and heading", () => {
  it("lists tape ticks around the value", () => {
    expect(tapeTicks(12.3, 10, 5)).toEqual([5, 10, 15, 20]);
  });

  it("wraps heading ticks through north", () => {
    expect(headingTicks(355, 10, 5).map((t) => t.deg)).toEqual([345, 350, 355, 0, 5]);
    expect(angleDelta(10, 350)).toBe(20);
    expect(angleDelta(350, 10)).toBe(-20);
  });
});

describe("flightPath", () => {
  it("is hidden below 1 m/s and with no velocity", () => {
    expect(flightPath(0.2, 0.1, 0, 0)).toBeNull();
    expect(flightPath(null, 1, 0, 0)).toBeNull();
  });

  it("derives drift and climb angle from NED velocity", () => {
    const fp = flightPath(10, 10, -10 * Math.SQRT2, 0)!;
    expect(fp.driftDeg).toBeCloseTo(45);
    expect(fp.fpaDeg).toBeCloseTo(45);
  });
});

describe("isInstrumentStale", () => {
  it("dims when not live or the last live sample is older than 2 s", () => {
    expect(isInstrumentStale(5000, 4000, true)).toBe(false);
    expect(isInstrumentStale(6500, 4000, true)).toBe(true);
    expect(isInstrumentStale(5000, 4900, false)).toBe(true);
    expect(isInstrumentStale(5000, null, true)).toBe(true);
  });
});
