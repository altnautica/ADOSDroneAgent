import { describe, expect, it } from "vitest";

import { fixedRequest } from "@/lib/gnss-model";

const rx = (fix: string, lat: number | null, lon: number | null, h: number | null) => ({
  fix,
  lat_deg: lat,
  lon_deg: lon,
  h_ellipsoid_m: h,
});

describe("setting a fixed base", () => {
  it("takes the first receiver with a 3D position", () => {
    expect(fixedRequest([rx("2d", 1, 2, 3), rx("3d", 51.5, -0.12, 45.2)])).toEqual({
      mode: "fixed",
      lat_deg: 51.5,
      lon_deg: -0.12,
      h_ellipsoid_m: 45.2,
    });
  });

  it("offers nothing without a known point", () => {
    expect(fixedRequest([])).toBeNull();
    expect(fixedRequest([rx("none", null, null, null), rx("3d", 51.5, -0.12, null)])).toBeNull();
  });
});
