import { describe, expect, it } from "vitest";

import {
  OCCUPANCY_DIM_MS,
  localDrone,
  localSceneInput,
  localTarget,
  minimapFrame,
  nextMinimapPin,
  panBy,
} from "@/lib/local-minimap";
import type { VehicleState } from "@/lib/types";
import { parsePilotState } from "@/shared/hud/cockpit-frames";
import { EMPTY_OCCUPANCY } from "@/shared/hud/local-frame";
import type { FollowState, NavPose, NavScan } from "@/shared/hud/nav-types";

const pose: NavPose = { R: [1, 0, 0, 0, 1, 0, 0, 0, 1], t: [4, 5, 0], health: "good", frame: "map", source: null };

function telemetry(lp: Partial<NonNullable<VehicleState["local_position"]>> | null, heading?: number): VehicleState {
  return {
    position: heading === undefined ? undefined : { heading },
    local_position: lp ? { x: 1, y: 2, z: -3, vx: 0, vy: 0, vz: 0, age_ms: 100, ...lp } : null,
  } as unknown as VehicleState;
}

function follow(over: Partial<FollowState>): FollowState {
  return {
    phase: "following",
    targetNed: [10, 20],
    targetVelNed: [1, 0],
    ...over,
  } as FollowState;
}

describe("minimapFrame", () => {
  it("goes local when the active set navigates on ExternalNav", () => {
    expect(
      minimapFrame({ pin: "auto", noFixSince: null, now: 0, ekfParams: { EK3_SRC1_POSXY: 6 }, activeSet: null }).frame,
    ).toBe("local");
  });
  it("follows the reported active set", () => {
    const params = { EK3_SRC1_POSXY: 3, EK3_SRC2_POSXY: 6 };
    expect(minimapFrame({ pin: "auto", noFixSince: null, now: 0, ekfParams: params, activeSet: 1 }).frame).toBe("global");
    expect(minimapFrame({ pin: "auto", noFixSince: null, now: 0, ekfParams: params, activeSet: 2 }).frame).toBe("local");
  });
  it("cycles the pin AUTO → LOCAL → MAP → AUTO", () => {
    expect([nextMinimapPin("auto"), nextMinimapPin("local"), nextMinimapPin("map")]).toEqual(["local", "map", "auto"]);
  });
});

describe("localDrone", () => {
  it("uses LOCAL_POSITION_NED with the FC heading", () => {
    expect(localDrone(telemetry({}, 90), true, null)).toEqual({ north: 1, east: 2, yawDeg: 90 });
  });
  it("falls back to the SLAM pose when the FC position is stale", () => {
    expect(localDrone(telemetry({ age_ms: 5000 }), true, { data: pose, ageMs: 10 })).toEqual({
      north: 4,
      east: 5,
      yawDeg: 0,
    });
  });
  it("draws nothing when no source is fresh or the link is down", () => {
    expect(localDrone(telemetry({ age_ms: 5000 }), true, { data: pose, ageMs: 5000 })).toBeNull();
    expect(localDrone(telemetry({}), false, null)).toBeNull();
  });
});

describe("localTarget", () => {
  it("places the follow target with a velocity tick", () => {
    expect(localTarget({ data: follow({}), ageMs: 0 })).toEqual({
      north: 10,
      east: 20,
      velNorth: 1,
      velEast: 0,
      dashed: false,
    });
  });
  it("dashes while coasting or searching", () => {
    expect(localTarget({ data: follow({ phase: "coasting" }), ageMs: 0 })?.dashed).toBe(true);
    expect(localTarget({ data: follow({ phase: "searching" }), ageMs: 0 })?.dashed).toBe(true);
  });
  it("hides a lost, idle, positionless or stale target", () => {
    expect(localTarget({ data: follow({ phase: "lost" }), ageMs: 0 })).toBeNull();
    expect(localTarget({ data: follow({ phase: "idle" }), ageMs: 0 })).toBeNull();
    expect(localTarget({ data: follow({ targetNed: null }), ageMs: 0 })).toBeNull();
    expect(localTarget({ data: follow({}), ageMs: 10_000 })).toBeNull();
  });
});

describe("localSceneInput", () => {
  const scan: NavScan = { frame: "body_frd", angleStartRad: 0, angleIncrementRad: Math.PI / 2, ranges: [2] };
  const base = {
    width: 200,
    height: 100,
    pxPerM: 10,
    pan: null,
    now: 10_000,
    drone: { north: 3, east: 4, yawDeg: 0 },
    target: null,
    pose: { data: pose, ageMs: 0 },
    scan: { data: scan, ageMs: 0 },
    occupancy: EMPTY_OCCUPANCY,
    occupancyAt: 10_000,
    trail: [],
    pilot: null,
  };

  it("centres on the drone and puts the scan in the map frame", () => {
    const input = localSceneInput(base);
    expect(input.view).toEqual({ centreX: 3, centreY: 4, pxPerM: 10, cx: 100, cy: 50 });
    expect(input.scanPoints).toEqual([[6, 5]]);
    expect(input.occupancyStale).toBe(false);
  });
  it("drops a stale scan and dims an old occupancy poll", () => {
    const input = localSceneInput({ ...base, scan: { data: scan, ageMs: 5000 }, occupancyAt: 10_000 - OCCUPANCY_DIM_MS - 1 });
    expect(input.scanPoints).toBeNull();
    expect(input.occupancyStale).toBe(true);
    expect(localSceneInput({ ...base, occupancyAt: null }).occupancyStale).toBe(true);
  });
  it("draws the pilot's plan only while its frame is fresh", () => {
    const data = parsePilotState({
      owner: "goto",
      plan_ned: [[3, 4], [8, 4]],
      goal_ned: [8, 4, -5],
      avoid_state: "blocked",
    })!;
    expect(localSceneInput({ ...base, pilot: { data, at: 9_000 } }).plan).toEqual({
      points: [[3, 4], [8, 4]],
      goal: [8, 4],
      blocked: true,
    });
    expect(localSceneInput({ ...base, pilot: { data, at: 7_000 } }).plan).toBeNull();
    expect(localSceneInput(base).plan).toBeNull();
  });
});

describe("panBy", () => {
  it("drags the map: right moves the view west, down moves it north", () => {
    expect(panBy({ north: 0, east: 0 }, 20, 10, 10, 5)).toEqual({ north: 1, east: -2, at: 5 });
  });
});
