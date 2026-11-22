import { describe, expect, it } from "vitest";

import { EMPTY_OCCUPANCY } from "./local-frame";
import { buildLocalScene, clampZoom, followSceneTarget, localPlan, stepZoom, viewCentre, RECENTRE_AFTER_MS } from "./local-scene";
import { parseFollowState } from "./nav-types";
import { activePosxySource, nextNoFixSince, resolveMinimapMode, MINIMAP_NO_FIX_HOLD_MS } from "./minimap-mode";
import { parseCssColor, recolourOccupancy, withAlpha } from "./occupancy-bitmap";

describe("minimap mode", () => {
  it("tracks the start of a below-3D run", () => {
    expect(nextNoFixSince(null, 2, 100)).toBe(100);
    expect(nextNoFixSince(100, 1, 500)).toBe(100);
    expect(nextNoFixSince(100, 3, 600)).toBeNull();
    expect(nextNoFixSince(100, null, 600)).toBeNull();
  });

  it("goes local after the hold, or on a non-GNSS source; pins win", () => {
    const base = { pin: "auto" as const, noFixSince: 0, activePosxy: 3 };
    expect(resolveMinimapMode({ ...base, now: MINIMAP_NO_FIX_HOLD_MS }).frame).toBe("global");
    expect(resolveMinimapMode({ ...base, now: MINIMAP_NO_FIX_HOLD_MS + 1 })).toEqual({ frame: "local", reason: "noFix" });
    expect(resolveMinimapMode({ ...base, noFixSince: null, now: 0, activePosxy: 6 }).reason).toBe("posxySource");
    expect(resolveMinimapMode({ ...base, pin: "map", now: 1e9, activePosxy: 6 }).frame).toBe("global");
    expect(resolveMinimapMode({ ...base, pin: "local", noFixSince: null, now: 0 }).reason).toBe("pinned");
  });

  it("reads the active set's POSXY", () => {
    expect(activePosxySource({ EK3_SRC2_POSXY: 6 }, 2)).toBe(6);
    expect(activePosxySource({ EK3_SRC2_POSXY: 6 }, 1)).toBeNull();
    expect(activePosxySource(null, 1)).toBeNull();
  });
});

describe("local scene", () => {
  it("clamps and steps zoom", () => {
    expect(clampZoom(1000)).toBe(80);
    expect(stepZoom(10, 1)).toBeCloseTo(12.5);
    expect(stepZoom(10, -1)).toBeCloseTo(8);
  });

  it("holds the pan, then recentres on the drone", () => {
    const pan = { north: 1, east: 2, at: 0 };
    const drone = { north: 9, east: 9 };
    expect(viewCentre(pan, drone, RECENTRE_AFTER_MS - 1)).toEqual({ north: 1, east: 2 });
    expect(viewCentre(pan, drone, RECENTRE_AFTER_MS)).toEqual(drone);
    expect(viewCentre(null, null, 0)).toEqual({ north: 0, east: 0 });
  });

  it("projects the drone, target tick, scan and origin", () => {
    const scene = buildLocalScene({
      width: 100,
      height: 80,
      view: { centreX: 0, centreY: 0, pxPerM: 10, cx: 50, cy: 40 },
      occupancy: EMPTY_OCCUPANCY,
      occupancyStale: true,
      trail: [],
      scanPoints: [[1, 0], [100, 0]],
      target: { north: 2, east: 1, velNorth: 1, velEast: 0, dashed: true },
      drone: { north: 0, east: 0, yawDeg: 45 },
      plan: null,
      movers: [],
    });
    expect(scene.drone).toEqual({ sx: 50, sy: 40, yawDeg: 45 });
    expect(scene.target).toEqual({
      sx: 60,
      sy: 20,
      tickX: 60,
      tickY: 10,
      dashed: true,
      searchArea: null,
      vantage: null,
      predictedPath: [],
    });
    expect(scene.scan).toEqual([[50, 30]]);
    expect(scene.origin).toEqual({ sx: 50, sy: 40 });
    expect(scene.dim).toBe(true);
    expect(scene.scaleBar).toEqual({ m: 5, px: 50 });
  });

  it("projects the pilot's planned path and goal", () => {
    const s = buildLocalScene({
      width: 200,
      height: 200,
      view: { centreX: 0, centreY: 0, pxPerM: 10, cx: 100, cy: 100 },
      occupancy: EMPTY_OCCUPANCY,
      occupancyStale: false,
      trail: [],
      scanPoints: null,
      target: null,
      drone: { north: 0, east: 0, yawDeg: 0 },
      plan: { points: [[0, 0], [5, 0]], goal: [5, 0], blocked: false },
      movers: [],
    });
    expect(s.plan?.points).toHaveLength(2);
    expect(s.plan?.goal).toEqual(s.plan?.points[1]);
    expect(s.plan?.points[1][1]).toBeLessThan(s.plan!.points[0][1]); // north is up the screen
    expect(s.plan?.blocked).toBe(false);
  });

  it("carries the blocked flag and a goal with no path", () => {
    const s = buildLocalScene({
      width: 200,
      height: 200,
      view: { centreX: 0, centreY: 0, pxPerM: 10, cx: 100, cy: 100 },
      occupancy: EMPTY_OCCUPANCY,
      occupancyStale: false,
      trail: [],
      scanPoints: null,
      target: null,
      drone: null,
      plan: { points: [], goal: [0, 3], blocked: true },
      movers: [],
    });
    expect(s.plan).toEqual({ points: [], goal: [130, 100], blocked: true });
  });

  it("projects each mover with a 2 s velocity arrow, its sigma ring, and fades a stale one", () => {
    const s = buildLocalScene({
      width: 200,
      height: 200,
      view: { centreX: 0, centreY: 0, pxPerM: 10, cx: 100, cy: 100 },
      occupancy: EMPTY_OCCUPANCY,
      occupancyStale: false,
      trail: [],
      scanPoints: null,
      target: null,
      drone: null,
      plan: null,
      movers: [
        { id: 3, kind: "person", source: "vision", ne: [5, 0], velNe: [0, 1.5], sigmaM: 0.4, ageMs: 100 },
        { id: 7, kind: "unknown", source: "scan", ne: [0, -2], velNe: [0, 0], sigmaM: 0.2, ageMs: 1500 },
      ],
    });
    expect(s.movers).toEqual([
      { id: 3, kind: "person", sx: 100, sy: 50, leadX: 130, leadY: 50, ringPx: 4, faded: false },
      { id: 7, kind: "unknown", sx: 80, sy: 100, leadX: null, leadY: null, ringPx: 2, faded: true },
    ]);
  });

  it("reads the plan from a pilot frame", () => {
    expect(localPlan(null)).toBeNull();
    expect(localPlan({ planNed: [], goalNed: null, avoidState: "clear" })).toBeNull();
    expect(localPlan({ planNed: [[0, 0], [4, 1]], goalNed: [4, 1, -5], avoidState: "blocked" })).toEqual({
      points: [[0, 0], [4, 1]],
      goal: [4, 1],
      blocked: true,
    });
    expect(localPlan({ planNed: [], goalNed: [2, 2, -5], avoidState: "detour" })?.blocked).toBe(false);
  });

  it("projects the search area, the vantage and the predicted path while recovering", () => {
    const f = parseFollowState({
      phase: "searching",
      target_ned: [2, 1],
      target_vel_ned: [0, 0],
      search_area: { center_ne: [2, 1], semi_major_m: 3, semi_minor_m: 1, heading_deg: 0 },
      vantage_ne: [-1, 2],
      predicted_path_ne: [[2, 1], [3, 1]],
    })!;
    const target = followSceneTarget(f);
    expect(target?.dashed).toBe(true);
    const s = buildLocalScene({
      width: 100,
      height: 80,
      view: { centreX: 0, centreY: 0, pxPerM: 10, cx: 50, cy: 40 },
      occupancy: EMPTY_OCCUPANCY,
      occupancyStale: false,
      trail: [],
      scanPoints: null,
      target,
      drone: null,
      plan: null,
      movers: [],
    });
    // A north-pointing major axis turns the x radius up the screen.
    expect(s.target?.searchArea).toEqual({ sx: 60, sy: 20, rxPx: 30, ryPx: 10, rotationRad: -Math.PI / 2 });
    expect(s.target?.vantage).toEqual({ sx: 70, sy: 50 });
    expect(s.target?.predictedPath).toEqual([[60, 20], [60, 10]]);
  });

  it("dashes the target only while recovering and hides it while waiting", () => {
    const at = (phase: string) => followSceneTarget(parseFollowState({ phase, target_ned: [1, 1] })!);
    for (const p of ["predicting", "repositioning", "searching"]) expect(at(p)?.dashed, p).toBe(true);
    for (const p of ["acquiring", "following", "holding"]) expect(at(p)?.dashed, p).toBe(false);
    expect(at("following")?.searchArea).toBeNull();
    expect(at("idle")).toBeNull();
    expect(at("holding-await-operator")).toBeNull();
  });
});

describe("occupancy bitmap", () => {
  it("parses hex and rgb colours", () => {
    expect(parseCssColor("#fff")).toEqual([255, 255, 255, 255]);
    expect(parseCssColor("rgb(10, 20, 30)")).toEqual([10, 20, 30, 255]);
    expect(parseCssColor("hsl(0 0% 0%)")).toBeNull();
  });

  it("paints occupied, free and unknown greys", () => {
    const ink = [200, 100, 50, 255] as const;
    const data = new Uint8ClampedArray([0, 0, 0, 255, 254, 254, 254, 255, 205, 205, 205, 255, 0, 0, 0, 0]);
    recolourOccupancy(data, withAlpha(ink, 0.9), withAlpha(ink, 0.1));
    expect([...data]).toEqual([200, 100, 50, 230, 200, 100, 50, 26, 0, 0, 0, 0, 0, 0, 0, 0]);
  });
});
