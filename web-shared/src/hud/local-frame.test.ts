import { describe, expect, it } from "vitest";

import {
  EMPTY_OCCUPANCY,
  OCCUPANCY_TILE_LIMIT,
  TRAIL_MAX_POINTS,
  TRAIL_WINDOW_MS,
  appendTrail,
  mapToScreen,
  mergeOccupancy,
  occupancyClass,
  poseYawDeg,
  scaleBarStep,
  scanToMap,
  tileFootprint,
  trailRuns,
  type TrailPoint,
} from "./local-frame";
import type { NavPose, NavScan } from "./nav-types";

const live = (seq: number, tiles: Array<[number, number]>, resM = 0.05, tileCells = 64) => ({
  seq,
  frame: "map",
  resM,
  tileCells,
  tiles: tiles.map(([ix, iy]) => ({ ix, iy, pngB64: `p${seq}` })),
});

describe("occupancyClass (map_server greys)", () => {
  it("reads ≤ 100 occupied, ≥ 230 free, between unknown", () => {
    expect(occupancyClass(0)).toBe("occupied");
    expect(occupancyClass(100)).toBe("occupied");
    expect(occupancyClass(205)).toBe("unknown");
    expect(occupancyClass(230)).toBe("free");
    expect(occupancyClass(254)).toBe("free");
  });
});

describe("tileFootprint", () => {
  it("anchors the top-left corner at max north, min east", () => {
    const fp = tileFootprint(1, -2, 0.05, 64);
    expect(fp.sizeM).toBeCloseTo(3.2);
    expect(fp.anchorX).toBeCloseTo(6.4);
    expect(fp.anchorY).toBeCloseTo(-6.4);
  });
});

describe("mergeOccupancy", () => {
  it("keeps tiles by index with the newest PNG winning", () => {
    let g = mergeOccupancy(EMPTY_OCCUPANCY, live(1, [[0, 0], [0, 1]]));
    g = mergeOccupancy(g, live(2, [[0, 0]]));
    expect(Object.keys(g.tiles)).toEqual(["0,1", "0,0"]);
    expect(g.tiles["0,0"].pngB64).toBe("p2");
    expect(g.seq).toBe(2);
  });

  it("starts over when the sequence runs backwards or the grid changes", () => {
    const g = mergeOccupancy(EMPTY_OCCUPANCY, live(5, [[0, 0]]));
    expect(Object.keys(mergeOccupancy(g, live(1, [[1, 1]])).tiles)).toEqual(["1,1"]);
    expect(Object.keys(mergeOccupancy(g, live(6, [[2, 2]], 0.1)).tiles)).toEqual(["2,2"]);
  });

  it("returns the same grid for an empty poll", () => {
    const g = mergeOccupancy(EMPTY_OCCUPANCY, live(3, [[0, 0]]));
    expect(mergeOccupancy(g, { ...live(3, []), resM: null, tileCells: null })).toBe(g);
  });

  it("drops the least recently changed tiles past the limit", () => {
    const many: Array<[number, number]> = Array.from({ length: OCCUPANCY_TILE_LIMIT + 3 }, (_, i) => [i, 0]);
    const g = mergeOccupancy(EMPTY_OCCUPANCY, live(1, many));
    expect(Object.keys(g.tiles)).toHaveLength(OCCUPANCY_TILE_LIMIT);
    expect(g.tiles["0,0"]).toBeUndefined();
  });
});

describe("projection", () => {
  const view = { centreX: 10, centreY: 5, pxPerM: 4, cx: 50, cy: 40 };
  it("draws north up and east right", () => {
    expect(mapToScreen(view, 10, 5)).toEqual({ sx: 50, sy: 40 });
    expect(mapToScreen(view, 11, 5)).toEqual({ sx: 50, sy: 36 });
    expect(mapToScreen(view, 10, 6)).toEqual({ sx: 54, sy: 40 });
  });

  it("puts scan returns in the map frame through the pose", () => {
    // Yawed 90° (nose east), at (2, 3).
    const pose: NavPose = { R: [0, -1, 0, 1, 0, 0, 0, 0, 1], t: [2, 3, 0], health: "good", frame: "map", source: null };
    const scan: NavScan = { frame: "body_frd", angleStartRad: 0, angleIncrementRad: Math.PI / 2, ranges: [4, 0] };
    const [[n, e]] = scanToMap(scan, pose);
    expect(n).toBeCloseTo(2);
    expect(e).toBeCloseTo(7);
    expect(poseYawDeg(pose)).toBeCloseTo(90);
  });

  it("picks a 40–80 px scale step", () => {
    expect(scaleBarStep(10)).toBe(5);
    expect(scaleBarStep(40)).toBe(1);
    expect(scaleBarStep(2)).toBe(20);
  });
});

describe("SLAM trail", () => {
  const p = (x: number, at: number, health: TrailPoint["health"] = "good"): TrailPoint => ({ x, y: 0, at, health });

  it("decimates points closer than the spacing but keeps lost samples", () => {
    let t = appendTrail([], p(0, 0));
    t = appendTrail(t, p(0.1, 1));
    expect(t).toHaveLength(1);
    t = appendTrail(t, p(0.1, 2, "lost"));
    expect(t).toHaveLength(2);
  });

  it("forgets points older than the window and caps the length", () => {
    let t = appendTrail([], p(0, 0));
    t = appendTrail(t, p(5, TRAIL_WINDOW_MS + 1));
    expect(t).toHaveLength(1);
    let long: TrailPoint[] = [];
    for (let i = 0; i < TRAIL_MAX_POINTS + 10; i++) long = appendTrail(long, p(i, i));
    expect(long).toHaveLength(TRAIL_MAX_POINTS);
  });

  it("breaks runs at lost samples and dashes degraded ones", () => {
    const runs = trailRuns([p(0, 0), p(1, 1), p(2, 2, "lost"), p(3, 3), p(4, 4, "degraded"), p(5, 5, "degraded")]);
    expect(runs.map((r) => r.dashed)).toEqual([false, true]);
    expect(runs[1].points.map((q) => q.x)).toEqual([3, 4, 5]);
  });
});
