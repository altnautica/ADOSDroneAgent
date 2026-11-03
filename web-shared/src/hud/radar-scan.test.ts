import { describe, expect, it } from "vitest";

import type { NavScan } from "./nav-types";
import {
  autoRadarScale,
  cycleRadarScale,
  nearestScanM,
  radarBand,
  radarReadout,
  radarScanDots,
} from "./radar-scan";

const scan = (ranges: number[]): NavScan => ({
  frame: "body_frd",
  angleStartRad: 0,
  angleIncrementRad: (2 * Math.PI) / ranges.length,
  ranges,
});

describe("radar scale", () => {
  it("auto-picks 3 / 6 / 12 m from the nearest return", () => {
    expect(autoRadarScale(1)).toBe(3);
    expect(autoRadarScale(4)).toBe(6);
    expect(autoRadarScale(7)).toBe(12);
    expect(autoRadarScale(null)).toBe(12);
  });
  it("cycles 3 → 6 → 12 → auto → 3", () => {
    expect([3, 6, 12, "auto"].map((s) => cycleRadarScale(s as 3))).toEqual([6, 12, "auto", 3]);
  });
});

describe("radarScanDots", () => {
  it("draws forward up and right-hand returns to the right", () => {
    const dots = radarScanDots(scan([5, 5, 0, 0]), 10, 40, 50, 50);
    expect(dots[0].x).toBeCloseTo(50);
    expect(dots[0].y).toBeCloseTo(30);
    expect(dots[1].x).toBeCloseTo(70);
    expect(dots[1].y).toBeCloseTo(50);
  });
  it("decimates to the dot cap", () => {
    expect(radarScanDots(scan(Array(360).fill(1)), 3, 40, 50, 50, 120)).toHaveLength(120);
  });
});

describe("readout", () => {
  it("finds the nearest positive return", () => {
    expect(nearestScanM(scan([0, 3, 2.5, 0]))).toBe(2.5);
    expect(nearestScanM(scan([0, 0]))).toBeNull();
    expect(nearestScanM(null)).toBeNull();
  });
  it("bands danger < 2 m, caution ≤ 5 m, else clear", () => {
    expect(radarBand(1.9)).toBe("danger");
    expect(radarBand(2)).toBe("caution");
    expect(radarBand(5)).toBe("caution");
    expect(radarBand(5.1)).toBe("clear");
    expect(radarBand(null)).toBe("clear");
  });
  it("takes the nearest of every source and words it", () => {
    expect(radarReadout(4, null, 1.2)).toEqual({ range: "1.2 m", word: "DANGER", band: "danger" });
    expect(radarReadout(undefined, 7)).toEqual({ range: "7.0 m", word: "CLEAR", band: "clear" });
    expect(radarReadout(null, NaN, 0)).toEqual({ range: "—", word: null, band: null });
  });
});
