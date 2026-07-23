import { describe, expect, it } from "vitest";

import {
  bearingToXY,
  obstacleSectors,
  OBSTACLE_INVALID_CM,
  sectorArcPath,
  trackBearing,
} from "@/lib/radar-geometry";

describe("bearingToXY", () => {
  const cx = 50;
  const cy = 50;
  const r = 40;
  it("puts north up", () => {
    const p = bearingToXY(0, r, cx, cy);
    expect(p.x).toBeCloseTo(50, 5);
    expect(p.y).toBeCloseTo(10, 5);
  });
  it("puts east right", () => {
    const p = bearingToXY(90, r, cx, cy);
    expect(p.x).toBeCloseTo(90, 5);
    expect(p.y).toBeCloseTo(50, 5);
  });
  it("puts south down", () => {
    const p = bearingToXY(180, r, cx, cy);
    expect(p.x).toBeCloseTo(50, 5);
    expect(p.y).toBeCloseTo(90, 5);
  });
  it("puts west left", () => {
    const p = bearingToXY(270, r, cx, cy);
    expect(p.x).toBeCloseTo(10, 5);
    expect(p.y).toBeCloseTo(50, 5);
  });
});

describe("trackBearing", () => {
  it("reads due north from a northward velocity", () => {
    expect(trackBearing(2, 0)).toBeCloseTo(0, 5);
  });
  it("reads due east from an eastward velocity", () => {
    expect(trackBearing(0, 2)).toBeCloseTo(90, 5);
  });
  it("reads due south from a southward velocity", () => {
    expect(trackBearing(-2, 0)).toBeCloseTo(180, 5);
  });
  it("returns null below the movement threshold", () => {
    expect(trackBearing(0.1, 0.1)).toBeNull();
  });
  it("returns null for non-finite velocity", () => {
    expect(trackBearing(NaN, 1)).toBeNull();
    expect(trackBearing(undefined, 1)).toBeNull();
  });
});

describe("obstacleSectors", () => {
  it("classifies danger and caution and skips clear + sentinel readings", () => {
    // index 0: 150 cm danger, 1: 400 cm caution, 2: 900 cm clear (no sector),
    // 3: 65535 sentinel (skipped).
    const field = obstacleSectors([150, 400, 900, OBSTACLE_INVALID_CM], 5, 0);
    expect(field.sectors).toHaveLength(2);
    expect(field.sectors[0].severity).toBe("danger");
    expect(field.sectors[0].startDeg).toBe(0);
    expect(field.sectors[0].endDeg).toBe(5);
    expect(field.sectors[1].severity).toBe("caution");
    expect(field.sectors[1].startDeg).toBe(5);
    // closest is the danger reading.
    expect(field.closestCm).toBe(150);
  });

  it("honours the angle offset", () => {
    const field = obstacleSectors([150], 10, 45);
    expect(field.sectors[0].startDeg).toBe(45);
    expect(field.sectors[0].endDeg).toBe(55);
  });

  it("is empty for no data and reports no closest", () => {
    expect(obstacleSectors([], 5, 0)).toEqual({ sectors: [], closestCm: null });
    expect(obstacleSectors(null, 5, 0)).toEqual({ sectors: [], closestCm: null });
    // all-clear readings → no sectors, no closest within the caution ring.
    expect(obstacleSectors([900, 1200], 5, 0)).toEqual({
      sectors: [],
      closestCm: null,
    });
  });
});

describe("sectorArcPath", () => {
  it("produces a closed annular-wedge path", () => {
    const d = sectorArcPath(0, 30, 12, 42, 50, 50);
    expect(d.startsWith("M ")).toBe(true);
    expect(d.trim().endsWith("Z")).toBe(true);
    expect(d).toContain("A 42 42");
    expect(d).toContain("A 12 12");
  });
});
