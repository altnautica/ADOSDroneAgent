import { describe, expect, it } from "vitest";

import {
  FLY_HERE_HOLD_MS,
  FLY_HERE_PRESS_MS,
  enToLatLon,
  fcOrigin,
  flyHereArgs,
  flyHerePosition,
  launchGoto,
  localToLatLon,
  mapPickAction,
  offsetFromDrone,
  unprojectEN,
} from "@/lib/fly-here";
import { freshLocalPosition, screenToLocal } from "@/lib/local-minimap";
import { computeMapView, latLonToEN, projectEN } from "@/lib/minimap-geometry";
import layoutJson from "@/shared/cockpit-spec/layout.json";
import type { PilotState } from "@/shared/hud/cockpit-frames";
import { mapToScreen } from "@/shared/hud/local-frame";
import { specText } from "@/shared/hud/spec-text";
import type { VehicleState } from "@/lib/types";

const pilot = (over: Partial<PilotState>): PilotState =>
  ({ controlMode: "ados", surfaceOps: ["goto"], armed: true, landed: false, ...over }) as unknown as PilotState;

describe("fly here", () => {
  it("warns from the spec that the hold takes over a running mission or skill", () => {
    expect(specText("map.fly_here.takes_over")).toMatch(/takes over/i);
  });

  it("reads the long-press and the confirm hold from the layout spec", () => {
    expect(FLY_HERE_PRESS_MS).toBe(layoutJson.map_long_press_ms);
    expect(FLY_HERE_HOLD_MS).toBe(layoutJson.holds_ms.fly_here);
  });

  it("opens Fly Here in the air, the Launch sheet on the ground, nothing on a stale pilot", () => {
    expect(mapPickAction(pilot({}), 100)).toBe("fly_here");
    expect(mapPickAction(pilot({ armed: false, landed: true }), 100)).toBe("launch");
    expect(mapPickAction(pilot({ armed: true, landed: true }), 100)).toBe("launch");
    expect(mapPickAction(pilot({}), 5000)).toBeNull();
    expect(mapPickAction(pilot({ armed: false, landed: true }), 5000)).toBeNull();
    expect(mapPickAction(null, 0)).toBeNull();
    // In the air without a surface goto: nothing to offer.
    expect(mapPickAction(pilot({ surfaceOps: [] }), 100)).toBeNull();
  });

  it("prefills the Launch go-to with whole metres north/east of the vehicle", () => {
    const drone = { lat: 12.97, lon: 77.59 };
    const target = enToLatLon(30.4, -12.6, drone);
    const off = offsetFromDrone(target, drone)!;
    expect(launchGoto(off)).toEqual({ n: -13, e: 30 });
    expect(offsetFromDrone(target, null)).toBeNull();
  });

  it("converts a local-frame point to lat/lon through the FC origin", () => {
    // The FC reports its global position as origin + local position.
    const origin = { lat: 47.3977, lon: 8.5456 };
    const local = { north: 40, east: -25 };
    const drone = localToLatLon(local.north, local.east, origin);
    const back = fcOrigin(drone, local);
    expect(back.lat).toBeCloseTo(origin.lat, 7);
    expect(back.lon).toBeCloseTo(origin.lon, 7);
    // A point picked 10 m north of the drone in the local frame is 10 m north of it.
    const picked = localToLatLon(local.north + 10, local.east, back);
    const en = latLonToEN(picked.lat, picked.lon, drone.lat, drone.lon);
    expect(en.n).toBeCloseTo(10, 3);
    expect(en.e).toBeCloseTo(0, 3);
  });

  it("maps a local-map pixel back to the local point drawn there", () => {
    const view = { centreX: 12, centreY: -4, pxPerM: 6, cx: 100, cy: 70 };
    const s = mapToScreen(view, 20, 3);
    const p = screenToLocal(view, s.sx, s.sy);
    expect(p.north).toBeCloseTo(20, 9);
    expect(p.east).toBeCloseTo(3, 9);
  });

  it("converts through the FC's local position only while it is fresh", () => {
    const t = (age: number) => ({ local_position: { x: 5, y: -2, z: 0, vx: 0, vy: 0, vz: 0, age_ms: age } }) as unknown as VehicleState;
    expect(freshLocalPosition(t(100), true)).toEqual({ north: 5, east: -2 });
    expect(freshLocalPosition(t(5000), true)).toBeNull();
    expect(freshLocalPosition(t(100), false)).toBeNull();
  });

  it("maps a point on the minimap back to the lat/lon drawn there", () => {
    const ref = { lat: 12.97, lon: 77.59 };
    const target = { lat: 12.9705, lon: 77.5893 };
    const en = latLonToEN(target.lat, target.lon, ref.lat, ref.lon);
    const view = computeMapView([{ e: 0, n: 0 }, en], 100, 12, 40)!;
    const pt = projectEN(en.e, en.n, view);
    const back = unprojectEN(pt.x, pt.y, view);
    const ll = enToLatLon(back.e, back.n, ref);
    expect(ll.lat).toBeCloseTo(target.lat, 6);
    expect(ll.lon).toBeCloseTo(target.lon, 6);
  });

  it("sends the current height above home, never below the floor", () => {
    expect(flyHereArgs({ lat: 1, lon: 2 }, 12.34)).toEqual({ lat: 1, lon: 2, alt_m: 12.3 });
    expect(flyHereArgs({ lat: 1, lon: 2 }, 0.5)?.alt_m).toBe(3);
  });

  it("refuses without a height reading rather than falling back to the floor", () => {
    expect(flyHereArgs({ lat: 1, lon: 2 }, null)).toBeNull();
    expect(flyHereArgs({ lat: 1, lon: 2 }, undefined)).toBeNull();
    expect(flyHereArgs({ lat: 1, lon: 2 }, Number.NaN)).toBeNull();
  });

  it("places the drone from the FC position through the EKF origin without a GNSS fix", () => {
    const pos = { lat: 12.97, lon: 77.59 };
    expect(flyHerePosition(pos, 3, true)).toEqual({ drone: pos, gnss: true });
    expect(flyHerePosition(pos, 0, true)).toEqual({ drone: pos, gnss: false });
    expect(flyHerePosition(pos, 1, true)).toEqual({ drone: pos, gnss: false });
    expect(flyHerePosition(pos, null, true)).toEqual({ drone: pos, gnss: true });
  });

  it("has no drone position (so no Fly Here) without a fix and without an EKF origin", () => {
    expect(flyHerePosition({ lat: 0, lon: 0 }, 0, true).drone).toBeNull();
    expect(flyHerePosition(null, 3, true).drone).toBeNull();
    expect(flyHerePosition({ lat: 12.97, lon: 77.59 }, 3, false).drone).toBeNull();
  });
});
