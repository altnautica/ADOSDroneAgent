import { describe, expect, it } from "vitest";

import { distanceFromHomeM, rowAgl } from "@/lib/bottom-row";
import type { VehicleState } from "@/lib/types";
import { CHIP_STALE_AFTER_MS } from "@/shared/hud/flight-state";

const pos = (lat: number | null, lon: number | null) => ({ lat, lon, alt_msl: null, alt_rel: 10, heading: null });
const local = (x: number, y: number, age_ms = 100) => ({ x, y, z: -10, vx: 0, vy: 0, vz: 0, age_ms });

describe("the bottom row's D (distance from home)", () => {
  it("uses the agent's own figure when it sends one", () => {
    expect(distanceFromHomeM({ home_distance: 42 } as VehicleState, null)).toBe(42);
  });

  it("measures from the home point to the GNSS position", () => {
    const t = { position: pos(47.0009, 8) } as VehicleState;
    expect(distanceFromHomeM(t, { lat: 47, lon: 8 })).toBeCloseTo(100.1, 0);
  });

  it("shows a distance in GNSS-denied flight from the local position's EKF origin", () => {
    const t = { position: pos(null, null), local_position: local(30, 40) } as VehicleState;
    expect(distanceFromHomeM(t, null)).toBe(50);
    // A zero lat/lon is no fix, not a position.
    expect(distanceFromHomeM({ position: pos(0, 0), local_position: local(6, 8) } as VehicleState, { lat: 47, lon: 8 })).toBe(10);
  });

  it("reads no data with a stale local position and no fix", () => {
    expect(distanceFromHomeM({ local_position: local(30, 40, 5000) } as VehicleState, null)).toBeNull();
    expect(distanceFromHomeM(null, null)).toBeNull();
  });
});

describe("the bottom row's AGL against a stalled pilot", () => {
  const pilot = (at: number) => ({ data: { aglM: 12.5, aglSource: "rangefinder" as const }, at });

  it("shows the pilot's height while the frame is fresh", () => {
    expect(rowAgl(pilot(1000), null, 1000 + CHIP_STALE_AFTER_MS)).toEqual({ m: 12.5, mark: null, stale: false });
  });

  it("dims to a dash once the pilot frame is older than stale_after_ms", () => {
    expect(rowAgl(pilot(1000), { aglM: 9, aglSource: "baro" }, 1001 + CHIP_STALE_AFTER_MS)).toEqual({ m: null, mark: null, stale: true });
  });

  it("falls back to the follow kind only when no pilot runs", () => {
    expect(rowAgl(null, { aglM: 9, aglSource: "baro" }, 5000)).toEqual({ m: 9, mark: "BARO", stale: false });
  });
});
