import { describe, expect, it } from "vitest";

import {
  computeMapView,
  distanceMeters,
  isValidFix,
  latLonToEN,
  projectEN,
  viewSpanMeters,
} from "@/lib/minimap-geometry";

describe("latLonToEN", () => {
  it("is the origin at the reference", () => {
    const en = latLonToEN(12.9, 77.6, 12.9, 77.6);
    expect(en.e).toBeCloseTo(0, 6);
    expect(en.n).toBeCloseTo(0, 6);
  });

  it("maps +0.001° latitude to ~111 m north", () => {
    const en = latLonToEN(12.901, 77.6, 12.9, 77.6);
    expect(en.e).toBeCloseTo(0, 3);
    expect(en.n).toBeCloseTo(111.32, 0); // ~111.32 m per 0.001°
  });

  it("scales east by cos(lat)", () => {
    // At 60°N, one degree of longitude is ~half the equatorial value.
    const eq = latLonToEN(0, 1, 0, 0).e;
    const hi = latLonToEN(60, 1, 60, 0).e;
    expect(hi).toBeCloseTo(eq * Math.cos(60 * (Math.PI / 180)), 0);
  });
});

describe("distanceMeters", () => {
  it("measures a short hop", () => {
    const d = distanceMeters({ lat: 12.9, lon: 77.6 }, { lat: 12.901, lon: 77.6 });
    expect(d).toBeCloseTo(111.32, 0);
  });

  it("is zero for the same point", () => {
    expect(distanceMeters({ lat: 1, lon: 2 }, { lat: 1, lon: 2 })).toBe(0);
  });
});

describe("isValidFix", () => {
  it("accepts a real position", () => {
    expect(isValidFix(12.9, 77.6)).toBe(true);
  });
  it("rejects the (0,0) no-fix sentinel", () => {
    expect(isValidFix(0, 0)).toBe(false);
  });
  it("rejects non-finite and out-of-range values", () => {
    expect(isValidFix(NaN, 10)).toBe(false);
    expect(isValidFix(95, 10)).toBe(false);
    expect(isValidFix(10, 200)).toBe(false);
    expect(isValidFix("12" as unknown, 77)).toBe(false);
  });
});

describe("computeMapView", () => {
  it("returns null for no points", () => {
    expect(computeMapView([], 100, 10, 40)).toBeNull();
  });

  it("centres a single point and floors the span at minSpanM", () => {
    const v = computeMapView([{ e: 5, n: -3 }], 100, 10, 40)!;
    expect(v).not.toBeNull();
    expect(v.centerE).toBe(5);
    expect(v.centerN).toBe(-3);
    // span floored to 40 → scale = (100 - 20) / 40 = 2 svg units per metre.
    expect(v.scale).toBeCloseTo(2, 6);
    const p = projectEN(5, -3, v);
    expect(p.x).toBeCloseTo(50, 6);
    expect(p.y).toBeCloseTo(50, 6);
  });

  it("fits a spread of points inside the padded box", () => {
    const v = computeMapView(
      [
        { e: -100, n: -100 },
        { e: 100, n: 100 },
      ],
      100,
      10,
      40,
    )!;
    // span 200 → scale = 80/200 = 0.4; centre at origin.
    expect(v.scale).toBeCloseTo(0.4, 6);
    const nw = projectEN(-100, 100, v);
    expect(nw.x).toBeCloseTo(10, 6); // left edge after padding
    expect(nw.y).toBeCloseTo(10, 6); // top edge (north is up)
  });
});

describe("projectEN", () => {
  it("puts north up (smaller y)", () => {
    const v = computeMapView([{ e: 0, n: 0 }], 100, 10, 40)!;
    const north = projectEN(0, 10, v);
    const south = projectEN(0, -10, v);
    expect(north.y).toBeLessThan(south.y);
  });
});

describe("viewSpanMeters", () => {
  it("reports the metres across the full view width", () => {
    const v = computeMapView([{ e: 0, n: 0 }], 100, 10, 40)!;
    // scale 2 svg/m → 100 svg units span 50 m.
    expect(viewSpanMeters(v, 100)).toBeCloseTo(50, 6);
  });
});
