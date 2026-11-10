// "Fly here" from a map: a long-press (or a right-click) on the map picks a
// point, a hold confirms it, and the cockpit sends the pilot's `goto` action
// with `{lat, lon, alt_m}` (altitude above home). The agent advertises the
// action in the pilot kind's `surface_ops`; without it the map offers nothing
// in the air. On the ground the same gesture opens the Launch sheet with the
// point as its go-to.

import layoutJson from "@/shared/cockpit-spec/layout.json";
import type { PilotState } from "@/shared/hud/cockpit-frames";
import { flyHereOffered } from "@/shared/hud/fly-here-offer";
import { CHIP_STALE_AFTER_MS } from "@/shared/hud/flight-state";
import { isValidFix, latLonToEN, type LatLon, type MapView } from "@/lib/minimap-geometry";

/** Press this long on a map, without moving, to pick a point
 *  (`layout.json` `map_long_press_ms`). */
export const FLY_HERE_PRESS_MS: number = layoutJson.map_long_press_ms;
/** The confirm hold (`layout.json` `holds_ms.fly_here`). */
export const FLY_HERE_HOLD_MS: number = layoutJson.holds_ms.fly_here;
/** A press that moves farther than this (CSS px) is not a long-press. */
export const FLY_HERE_SLOP_PX = 8;
/** The lowest height a "Fly here" sends (metres above home). */
export const FLY_HERE_MIN_ALT_M = 3;

const EARTH_R = 6378137;
const DEG = Math.PI / 180;

/** What a picked map point does: the Fly Here confirm in the air, the Launch
 *  sheet with a go-to on the ground, or nothing (a stale pilot state, or a
 *  pilot that refuses a surface goto now). */
export type MapPickAction = "fly_here" | "launch" | null;

export function mapPickAction(p: PilotState | null, pilotAgeMs: number): MapPickAction {
  if (flyHereOffered(p, pilotAgeMs)) return "fly_here";
  if (p !== null && pilotAgeMs < CHIP_STALE_AFTER_MS && (p.landed || !p.armed)) return "launch";
  return null;
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
 *  home (never below the floor). Null while the cockpit has no height reading:
 *  a guessed height could send the drone down to the floor, so it refuses. */
export function flyHereArgs(target: LatLon, altRelM: number | null | undefined): { lat: number; lon: number; alt_m: number } | null {
  if (typeof altRelM !== "number" || !Number.isFinite(altRelM)) return null;
  return { lat: target.lat, lon: target.lon, alt_m: Math.max(FLY_HERE_MIN_ALT_M, Math.round(altRelM * 10) / 10) };
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
