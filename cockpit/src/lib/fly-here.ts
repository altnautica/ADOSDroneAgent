// "Fly here" from the minimap: a long-press on the map picks a point, a hold
// confirms it, and the cockpit sends the pilot's `goto` action with
// `{lat, lon, alt_m}` (altitude above home). The agent advertises the action in
// the pilot kind's `surface_ops`; without it the map offers nothing.

import { isValidFix, type LatLon, type MapView } from "@/lib/minimap-geometry";

/** Press this long on the map, without moving, to pick a point. */
export const FLY_HERE_PRESS_MS = 600;
/** A press that moves farther than this (CSS px) is not a long-press. */
export const FLY_HERE_SLOP_PX = 8;
/** The lowest height a "Fly here" sends (metres above home). */
export const FLY_HERE_MIN_ALT_M = 3;

const EARTH_R = 6378137;
const DEG = Math.PI / 180;

/** The confirm sheet's warning: the hold replaces what the drone is doing. */
export const FLY_HERE_TAKES_OVER = "Takes over from any running mission or skill.";

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
