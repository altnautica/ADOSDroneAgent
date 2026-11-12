// "Fly here" from a map: a long-press (or a right-click) on the map picks a
// point, a hold confirms it, and the cockpit sends it by one of two routes
// (shared `flyHereOffered`): the pilot's `goto` action with `{lat, lon,
// alt_m}` (altitude above home) while the pilot accepts a surface goto, else
// a reposition straight to a flight controller in GUIDED that is armed and
// flying. On the ground the same gesture opens the Launch sheet with the
// point as its go-to.

import layoutJson from "@/shared/cockpit-spec/layout.json";
import type { PilotState } from "@/shared/hud/cockpit-frames";
import { flyHereOffered, type FlyHereFc, type FlyHereReason } from "@/shared/hud/fly-here-offer";
import { CHIP_STALE_AFTER_MS } from "@/shared/hud/flight-state";
import { isValidFix, latLonToEN, type LatLon, type MapView } from "@/lib/minimap-geometry";
import type { VehicleState } from "@/lib/types";
import { ackText } from "@/lib/mav-result";
import { sendCommand, type CommandResponse } from "@/lib/api";
import { ApiError, errorDetail, isAuthChallenge } from "@/shared/api-fetch";
import { refusalText } from "@/shared/hud/refusal";
import { hasSpecText, specText } from "@/shared/hud/spec-text";

/** Press this long on a map, without moving, to pick a point
 *  (`layout.json` `map_long_press_ms`). */
export const FLY_HERE_PRESS_MS: number = layoutJson.map_long_press_ms;
/** The confirm hold (`layout.json` `holds_ms.fly_here`). */
export const FLY_HERE_HOLD_MS: number = layoutJson.holds_ms.fly_here;
/** A press that moves farther than this (CSS px) is not a long-press. */
export const FLY_HERE_SLOP_PX = 8;
/** The lowest height a "Fly here" sends (metres above home). */
export const FLY_HERE_MIN_ALT_M = 3;
/** The highest height a "Fly here" sends: the agent refuses a reposition
 *  above the launch ceiling (`layout.json` `launch.alt_max_m`). */
export const FLY_HERE_MAX_ALT_M: number = layoutJson.launch.alt_max_m;

const EARTH_R = 6378137;
const DEG = Math.PI / 180;

/** What a picked map point does: the Fly Here confirm in the air, sent
 *  through the pilot (`pilot`) or straight to the flight controller (`fc`),
 *  the Launch sheet with a go-to on the ground, or nothing (a stale pilot
 *  state, or neither route takes a point now). Only a reported landed or
 *  disarmed state opens the Launch sheet; an unknown one is possibly
 *  airborne. */
export type MapPickAction = "pilot" | "fc" | "launch" | null;

export function mapPickAction(p: PilotState | null, pilotAgeMs: number, fc: FlyHereFc | null): MapPickAction {
  const { route } = flyHereOffered(p, pilotAgeMs, fc);
  if (route !== null) return route;
  if (p !== null && pilotAgeMs < CHIP_STALE_AFTER_MS && (p.landed === true || p.armed === false)) return "launch";
  return null;
}

/** Why a picked point does nothing, when the shared rule knows (a map
 *  long-press then says `refusal.<reason>` instead of opening a sheet), else
 *  null. Null too whenever the pick does something. */
export function mapPickRefusal(p: PilotState | null, pilotAgeMs: number, fc: FlyHereFc | null): FlyHereReason | null {
  if (mapPickAction(p, pilotAgeMs, fc) !== null) return null;
  return flyHereOffered(p, pilotAgeMs, fc).reason ?? null;
}

/** The flight controller's landed state from `/api/telemetry`
 *  `landed_state` (`EXTENDED_SYS_STATE`): null while unknown. A take-off or
 *  a landing in progress is off the ground. */
export function fcLanded(state: VehicleState["landed_state"]): boolean | null {
  if (state === "on_ground") return true;
  if (state === "in_air" || state === "takeoff" || state === "landing") return false;
  return null;
}

/** The flight controller's facts for the FC route: its mode is GUIDED (the
 *  pilot's report of the FC mode, else the vehicle snapshot's), it is armed,
 *  and whether it has landed: the fresh pilot's report, else the flight
 *  controller's own `landed_state`. With neither the facts are unknown and
 *  the FC route is not offered. `pilot` is the fresh pilot state or null. */
export function flyHereFc(t: VehicleState | null, live: boolean, pilot: PilotState | null): FlyHereFc | null {
  if (!live || !t) return null;
  const landed = pilot && pilot.landed !== null ? pilot.landed : fcLanded(t.landed_state);
  if (landed === null) return null;
  const mode = pilot?.fcModeName ?? t.mode ?? null;
  return { guided: mode?.toUpperCase() === "GUIDED", armed: t.armed === true, landed };
}

/** The Launch sheet's go-to for a picked point: whole metres north/east of
 *  the vehicle. */
export function launchGoto(fromDrone: { n: number; e: number }): { n: number; e: number } {
  return { n: Math.round(fromDrone.n), e: Math.round(fromDrone.e) };
}

/** The world point (metres east/north of the view's reference) under an SVG
 *  point: the inverse of `projectEN`. */
export function unprojectEN(x: number, y: number, v: MapView): { e: number; n: number } {
  return { e: v.centerE + (x - v.cx) / v.scale, n: v.centerN - (y - v.cy) / v.scale };
}

/** The lat/lon `e`, `n` metres from `ref` (inverse of `latLonToEN`). */
export function enToLatLon(e: number, n: number, ref: LatLon): LatLon {
  return {
    lat: ref.lat + n / EARTH_R / DEG,
    lon: ref.lon + e / (EARTH_R * Math.cos(ref.lat * DEG)) / DEG,
  };
}

/** Metres north/east of `drone` to `target`, or null without a drone. */
export function offsetFromDrone(target: LatLon, drone: LatLon | null): { n: number; e: number } | null {
  if (!drone) return null;
  const en = latLonToEN(target.lat, target.lon, drone.lat, drone.lon);
  return { n: en.n, e: en.e };
}

/** The `goto` arguments: the picked point at the drone's current height above
 *  home, held between the floor and the launch ceiling. Null while the cockpit
 *  has no height reading: a guessed height could send the drone down to the
 *  floor, so it refuses. */
export function flyHereArgs(target: LatLon, altRelM: number | null | undefined): { lat: number; lon: number; alt_m: number } | null {
  if (typeof altRelM !== "number" || !Number.isFinite(altRelM)) return null;
  const alt = Math.min(FLY_HERE_MAX_ALT_M, Math.max(FLY_HERE_MIN_ALT_M, Math.round(altRelM * 10) / 10));
  return { lat: target.lat, lon: target.lon, alt_m: alt };
}

/** Send a picked point to a flight controller in GUIDED: `POST /api/command`
 *  `reposition` with `[lat, lon, alt_m]` (MAVLink `DO_REPOSITION`, altitude
 *  above home). Resolves null once the FC accepts it, else the refusal in the
 *  spec's words: the FC's answer (`mav_result.*` or its status text), no FC
 *  link, a value the route refuses, a route this FC lacks, a refused request,
 *  the agent's own reason, or (only when nothing answered) no answer. */
export async function sendFcReposition(args: { lat: number; lon: number; alt_m: number }): Promise<string | null> {
  let res: CommandResponse;
  try {
    res = await sendCommand("reposition", [args.lat, args.lon, args.alt_m]);
  } catch (e) {
    return e instanceof ApiError ? apiRefusal(e) : refusalText("no response");
  }
  const ack = res.ack;
  if (!ack?.observed) return refusalText("no response");
  return ack.accepted ? null : (ackText(ack) ?? specText("refusal.refused"));
}

/** The refusal for an HTTP answer from the command route: the agent did
 *  answer, so this is never "no answer". */
function apiRefusal(e: ApiError): string {
  if (e.status === 503) return refusalText("no_fc");
  if (e.status === 400) return refusalText("bad_args");
  if (e.status === 501) return refusalText("unsupported");
  if (isAuthChallenge(e.status)) return specText("refusal.not_authorized");
  const code = e.code;
  if (code && hasSpecText(`refusal.${code}`) && !specText(`refusal.${code}`).includes("{")) return refusalText(code);
  return errorDetail(e.body, specText("refusal.refused"));
}

/** Where the global minimap places the drone, and whether that position comes
 *  from a GNSS fix. The flight controller's global position also holds without
 *  a fix while its EKF has an origin (it reports the local position through the
 *  origin), and the pilot maps a `goto` through the same frame, so the drone
 *  marker and "Fly here" use it as Mission Control does. Without a fix and
 *  without an origin the FC reports (0, 0): no position, so no "Fly here". */
export function flyHerePosition(
  pos: { lat?: number | null; lon?: number | null } | null | undefined,
  fixType: number | null | undefined,
  live: boolean,
): { drone: LatLon | null; gnss: boolean } {
  if (!live || !pos || !isValidFix(pos.lat, pos.lon)) return { drone: null, gnss: false };
  const drone = { lat: pos.lat as number, lon: pos.lon as number };
  // A null fix type (some republished snapshots) falls back to coord validity.
  return { drone, gnss: fixType == null || fixType >= 2 };
}

/** The FC's EKF origin: its global position (`flyHerePosition`) less its
 *  local position (LOCAL_POSITION_NED north/east, metres). The local-frame
 *  map converts through it, so a picked local point becomes the lat/lon the
 *  pilot maps back through the same origin. */
export function fcOrigin(drone: LatLon, local: { north: number; east: number }): LatLon {
  return enToLatLon(-local.east, -local.north, drone);
}

/** A local-frame point (metres north/east of the EKF origin) as lat/lon. */
export function localToLatLon(north: number, east: number, origin: LatLon): LatLon {
  return enToLatLon(east, north, origin);
}
