import { describe, expect, it } from "vitest";

import { buildRadarView, northUpScan, SCAN_FRESH_MS } from "@/lib/radar-scan-view";
import type { NavScan } from "@/shared/hud/nav-types";

/** Four returns at 0°, 90°, 180°, 270° body. */
function scan(ranges: number[]): NavScan {
  return { frame: "body_frd", angleStartRad: 0, angleIncrementRad: Math.PI / 2, ranges };
}

const BASE = {
  scanAgeMs: 0,
  headingDeg: null,
  setting: "auto" as const,
  fcNearestM: null,
  fcPresent: false,
  minObstacleM: null,
  navAgeMs: Infinity,
  radiusPx: 40,
  cx: 50,
  cy: 50,
};

describe("northUpScan", () => {
  it("rotates body angles by the heading", () => {
    expect(northUpScan(scan([1]), 90).angleStartRad).toBeCloseTo(Math.PI / 2);
  });
  it("leaves the scan nose-up without a heading", () => {
    const s = scan([1]);
    expect(northUpScan(s, null)).toBe(s);
  });
});

describe("buildRadarView", () => {
  it("draws a forward return straight up the dial with no heading", () => {
    const v = buildRadarView({ ...BASE, scan: scan([6, 0, 0, 0]), setting: 12 });
    expect(v.dots).toHaveLength(1);
    expect(v.dots[0].x).toBeCloseTo(50);
    expect(v.dots[0].y).toBeCloseTo(30);
  });

  it("puts a forward return east on the north-up dial when heading east", () => {
    const v = buildRadarView({ ...BASE, scan: scan([6, 0, 0, 0]), setting: 12, headingDeg: 90 });
    expect(v.dots[0].x).toBeCloseTo(70);
    expect(v.dots[0].y).toBeCloseTo(50);
  });

  it("drops returns beyond the display range", () => {
    const v = buildRadarView({ ...BASE, scan: scan([2, 4, 10, 0]), setting: 3 });
    expect(v.dots).toHaveLength(1);
  });

  it("auto-scales from the nearest return", () => {
    expect(buildRadarView({ ...BASE, scan: scan([1, 0, 0, 0]) }).scaleM).toBe(3);
    expect(buildRadarView({ ...BASE, scan: scan([4, 0, 0, 0]) }).scaleM).toBe(6);
    expect(buildRadarView({ ...BASE, scan: scan([9, 0, 0, 0]) }).scaleLabel).toBe("AUTO 12 m");
    expect(buildRadarView({ ...BASE, scan: scan([9, 0, 0, 0]), setting: 6 }).scaleLabel).toBe("6 m");
  });

  it("draws nothing from a stale scan", () => {
    const v = buildRadarView({ ...BASE, scan: scan([1, 1, 1, 1]), scanAgeMs: SCAN_FRESH_MS + 1 });
    expect(v.dots).toHaveLength(0);
    expect(v.readout).toBeNull();
  });

  it("reads DANGER, CAUTION and CLEAR from the nearest of every source", () => {
    expect(buildRadarView({ ...BASE, scan: scan([1.5, 0, 0, 0]) }).readout).toEqual({
      range: "1.5 m",
      word: "DANGER",
      band: "danger",
    });
    expect(
      buildRadarView({ ...BASE, scan: scan([8, 0, 0, 0]), minObstacleM: 3.2, navAgeMs: 0 }).readout,
    ).toEqual({ range: "3.2 m", word: "CAUTION", band: "caution" });
    expect(buildRadarView({ ...BASE, scan: scan([8, 0, 0, 0]) }).readout?.word).toBe("CLEAR");
  });

  it("says CLEAR when a live source has no return, and nothing with no source", () => {
    expect(buildRadarView({ ...BASE, scan: scan([0, 0, 0, 0]) }).readout).toEqual({
      range: "—",
      word: "CLEAR",
      band: "clear",
    });
    expect(buildRadarView({ ...BASE, scan: null, fcPresent: true }).readout?.word).toBe("CLEAR");
    expect(buildRadarView({ ...BASE, scan: null }).readout).toBeNull();
  });

  it("ignores a stale companion minimum", () => {
    const v = buildRadarView({ ...BASE, scan: null, minObstacleM: 1, navAgeMs: 10_000 });
    expect(v.readout).toBeNull();
  });
});
