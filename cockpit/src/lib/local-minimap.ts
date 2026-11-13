// The local-frame minimap's inputs, from what the cockpit holds: the frame
// choice (AUTO switches to local after 2 s below a 3D fix or when the active
// EKF horizontal source carries no GNSS; a LOCAL / MAP pin wins), the drone
// from the FC's LOCAL_POSITION_NED (the SLAM pose when the FC has none), the
// follow target, the pilot's planned path and goal, the scan in the map frame,
// and whether the occupancy layer is stale. Every source is age-gated so a
// dead link never draws a live marker.

import {
  poseYawDeg,
  scanToMap,
  type LocalView,
  type OccupancyGrid,
  type TrailPoint,
} from "@/shared/hud/local-frame";
import type { PilotState } from "@/shared/hud/cockpit-frames";
import {
  localPlan,
  viewCentre,
  type LocalSceneDrone,
  type LocalSceneInput,
  type LocalSceneTarget,
} from "@/shared/hud/local-scene";
import { activePosxySource, resolveMinimapMode, type MinimapFrame, type MinimapLocalReason, type MinimapPin } from "@/shared/hud/minimap-mode";
import type { FollowState, NavPose, NavScan } from "@/shared/hud/nav-types";
import { pilotFresh } from "@/shared/hud/pilot-fresh";
import type { VehicleState } from "@/lib/types";

/** LOCAL_POSITION_NED older than this is not drawn. */
export const LOCAL_POSITION_FRESH_MS = 2000;
/** A SLAM pose (30 Hz) older than this is not drawn. */
export const POSE_FRESH_MS = 1000;
/** The scan (10 Hz) older than this is not drawn. */
export const SCAN_LAYER_FRESH_MS = 1000;
/** Follow state older than this hides the target. */
export const TARGET_FRESH_MS = 2000;
/** Occupancy polls run at 1 Hz; older than this the layer is dimmed. */
export const OCCUPANCY_DIM_MS = 5000;

export interface MinimapFrameInput {
  pin: MinimapPin;
  noFixSince: number | null;
  now: number;
  ekfParams: Readonly<Record<string, number>> | null;
  /** The source set vision-nav reports active, else the one this cockpit
   *  last commanded, else null (set 1 is the FC's boot default). */
  activeSet: 1 | 2 | 3 | null;
}

export function minimapFrame(input: MinimapFrameInput): { frame: MinimapFrame; reason: MinimapLocalReason } {
  return resolveMinimapMode({
    pin: input.pin,
    noFixSince: input.noFixSince,
    now: input.now,
    activePosxy: activePosxySource(input.ekfParams, input.activeSet ?? 1),
  });
}

/** The next pin when the header toggle is pressed: AUTO → LOCAL → MAP → AUTO. */
export function nextMinimapPin(pin: MinimapPin): MinimapPin {
  return pin === "auto" ? "local" : pin === "local" ? "map" : "auto";
}

/** A heading that is a real compass bearing, else null. */
function headingOrNull(h: number | null | undefined): number | null {
  return h != null && Number.isFinite(h) && h >= 0 && h <= 360 ? h : null;
}

/** The drone in the local frame: LOCAL_POSITION_NED when fresh, else the SLAM
 *  pose; heading from the FC compass, else the pose yaw. */
export function localDrone(
  telemetry: VehicleState | null,
  live: boolean,
  pose: { data: NavPose; ageMs: number } | null,
): LocalSceneDrone | null {
  const fcHeading = live ? headingOrNull(telemetry?.position?.heading) : null;
  const freshPose = pose && pose.ageMs <= POSE_FRESH_MS ? pose.data : null;
  const lp = live ? telemetry?.local_position : null;
  if (lp && lp.age_ms <= LOCAL_POSITION_FRESH_MS && Number.isFinite(lp.x) && Number.isFinite(lp.y)) {
    return { north: lp.x, east: lp.y, yawDeg: fcHeading ?? (freshPose ? poseYawDeg(freshPose) : 0) };
  }
  if (freshPose) {
    return { north: freshPose.t[0], east: freshPose.t[1], yawDeg: fcHeading ?? poseYawDeg(freshPose) };
  }
  return null;
}

/** The follow target in the local frame; dashed while coasting or searching. */
export function localTarget(follow: { data: FollowState; ageMs: number } | null): LocalSceneTarget | null {
  if (!follow || follow.ageMs > TARGET_FRESH_MS) return null;
  const f = follow.data;
  if (!f.targetNed || f.phase === "idle" || f.phase === "lost") return null;
  return {
    north: f.targetNed[0],
    east: f.targetNed[1],
    velNorth: f.targetVelNed?.[0] ?? null,
    velEast: f.targetVelNed?.[1] ?? null,
    dashed: f.phase === "coasting" || f.phase === "searching",
  };
}

export interface LocalSceneSources {
  width: number;
  height: number;
  pxPerM: number;
  pan: { north: number; east: number; at: number } | null;
  now: number;
  drone: LocalSceneDrone | null;
  target: LocalSceneTarget | null;
  pose: { data: NavPose; ageMs: number } | null;
  scan: { data: NavScan; ageMs: number } | null;
  occupancy: OccupancyGrid;
  occupancyAt: number | null;
  trail: readonly TrailPoint[];
  /** The newest pilot frame and its receive time; its plan shows while fresh. */
  pilot: { data: PilotState; at: number } | null;
}

export function localSceneInput(src: LocalSceneSources): LocalSceneInput {
  const centre = viewCentre(src.pan, src.drone, src.now);
  const view: LocalView = {
    centreX: centre.north,
    centreY: centre.east,
    pxPerM: src.pxPerM,
    cx: src.width / 2,
    cy: src.height / 2,
  };
  const scanPoints =
    src.scan && src.pose && src.scan.ageMs <= SCAN_LAYER_FRESH_MS && src.pose.ageMs <= POSE_FRESH_MS
      ? scanToMap(src.scan.data, src.pose.data)
      : null;
  return {
    width: src.width,
    height: src.height,
    view,
    occupancy: src.occupancy,
    occupancyStale: src.occupancyAt === null || src.now - src.occupancyAt > OCCUPANCY_DIM_MS,
    trail: src.trail,
    scanPoints,
    target: src.target,
    drone: src.drone,
    plan: src.pilot && pilotFresh(src.pilot.at, src.now) ? localPlan(src.pilot.data) : null,
  };
}

/** The local-frame drag: screen pixels moved → the new pan centre. Dragging
 *  right moves the view west; dragging down moves it north. */
export function panBy(
  centre: { north: number; east: number },
  dxPx: number,
  dyPx: number,
  pxPerM: number,
  at: number,
): { north: number; east: number; at: number } {
  return { north: centre.north + dyPx / pxPerM, east: centre.east - dxPx / pxPerM, at };
}

/** The local-frame point (metres north/east of the origin) under a screen
 *  pixel: the inverse of `mapToScreen`. */
export function screenToLocal(view: LocalView, sx: number, sy: number): { north: number; east: number } {
  return { north: view.centreX - (sy - view.cy) / view.pxPerM, east: view.centreY + (sx - view.cx) / view.pxPerM };
}

/** The FC's LOCAL_POSITION_NED north/east while fresh, else null: the frame
 *  the local map converts to lat/lon through the EKF origin. */
export function freshLocalPosition(telemetry: VehicleState | null, live: boolean): { north: number; east: number } | null {
  const lp = live ? telemetry?.local_position : null;
  return lp && lp.age_ms <= LOCAL_POSITION_FRESH_MS && Number.isFinite(lp.x) && Number.isFinite(lp.y) ? { north: lp.x, east: lp.y } : null;
}
