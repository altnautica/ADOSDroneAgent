// Pure geometry for the drone-centric radar: a north-up compass whose live core
// is the vehicle's heading (nose bearing) and course-over-ground (the direction
// it is actually moving), both derived from telemetry the cockpit already reads.
//
// It can additionally paint surround-obstacle sectors — but ONLY from real
// OBSTACLE_DISTANCE data when the agent's sensor stack surfaces a proximity
// sensor onto the vehicle snapshot. The base wire carries none today, so the
// radar is a heading/track instrument first; obstacle sectors are an additive
// overlay that appears with real data and never implies a false "all clear".
// Everything here is pure so the angle + sector math is unit-testable.

const RAD = Math.PI / 180;

export interface RadarPoint {
  x: number;
  y: number;
}

/** North-up polar: bearing 0° = up (north), 90° = right (east), clockwise. `r`
 *  is the radius out from the centre. */
export function bearingToXY(
  bearingDeg: number,
  r: number,
  cx: number,
  cy: number,
): RadarPoint {
  const a = (bearingDeg - 90) * RAD;
  return { x: cx + r * Math.cos(a), y: cy + r * Math.sin(a) };
}

/**
 * Course over ground (deg, 0 = north, 90 = east) from the north/east velocity
 * components, or null when the vehicle is essentially stationary (ground speed
 * below `minSpeed` m/s) so a jittery bearing is never drawn.
 */
export function trackBearing(
  vNorth: number | null | undefined,
  vEast: number | null | undefined,
  minSpeed = 0.5,
): number | null {
  if (
    typeof vNorth !== "number" ||
    typeof vEast !== "number" ||
    !Number.isFinite(vNorth) ||
    !Number.isFinite(vEast)
  ) {
    return null;
  }
  if (Math.hypot(vNorth, vEast) < minSpeed) return null;
  let deg = Math.atan2(vEast, vNorth) / RAD;
  if (deg < 0) deg += 360;
  return deg;
}

/** MAVLink OBSTACLE_DISTANCE sentinels + the caution/danger thresholds (cm),
 *  matching the ground-control radar so the two surfaces agree. */
export const OBSTACLE_INVALID_CM = 65535;
export const OBSTACLE_DANGER_CM = 200;
export const OBSTACLE_CAUTION_CM = 500;

export type ObstacleSeverity = "danger" | "caution";

export interface ObstacleSector {
  startDeg: number;
  endDeg: number;
  severity: ObstacleSeverity;
}

export interface ObstacleField {
  sectors: ObstacleSector[];
  /** The nearest valid obstacle range in cm, or null when nothing is within the
   *  caution ring / no valid readings exist. */
  closestCm: number | null;
}

/**
 * Turn an OBSTACLE_DISTANCE ranging array into caution/danger sectors. A reading
 * that is non-finite, non-positive, or the 65535 "no reading" sentinel is
 * skipped (never treated as a zero-range obstacle), and only readings within the
 * caution ring produce a sector — so with no real hazard nothing is painted,
 * exactly like the ground-control radar.
 */
export function obstacleSectors(
  distancesCm: number[] | null | undefined,
  incrementDeg: number,
  angleOffsetDeg: number,
): ObstacleField {
  if (!Array.isArray(distancesCm) || distancesCm.length === 0) {
    return { sectors: [], closestCm: null };
  }
  const inc = incrementDeg > 0 ? incrementDeg : 5;
  const maxByArc = Math.floor(360 / inc);
  const count = Math.min(distancesCm.length, maxByArc > 0 ? maxByArc : distancesCm.length);
  const sectors: ObstacleSector[] = [];
  let closest = OBSTACLE_INVALID_CM;
  for (let i = 0; i < count; i++) {
    const d = distancesCm[i];
    if (typeof d !== "number" || !Number.isFinite(d) || d <= 0 || d >= OBSTACLE_INVALID_CM) {
      continue;
    }
    let severity: ObstacleSeverity | null = null;
    if (d < OBSTACLE_DANGER_CM) severity = "danger";
    else if (d <= OBSTACLE_CAUTION_CM) severity = "caution";
    // A reading beyond the caution ring is clear — it produces no sector and does
    // not count as the nearest hazard, so the readout never colours a "range"
    // that has no visible sector behind it.
    if (!severity) continue;
    if (d < closest) closest = d;
    const start = angleOffsetDeg + i * inc;
    sectors.push({ startDeg: start, endDeg: start + inc, severity });
  }
  return { sectors, closestCm: closest < OBSTACLE_INVALID_CM ? closest : null };
}

/** An annular-wedge SVG path for one sector, from `startDeg` to `endDeg` between
 *  `innerR` and `outerR` about `(cx, cy)`. */
export function sectorArcPath(
  startDeg: number,
  endDeg: number,
  innerR: number,
  outerR: number,
  cx: number,
  cy: number,
): string {
  const o1 = bearingToXY(startDeg, outerR, cx, cy);
  const o2 = bearingToXY(endDeg, outerR, cx, cy);
  const i2 = bearingToXY(endDeg, innerR, cx, cy);
  const i1 = bearingToXY(startDeg, innerR, cx, cy);
  const large = endDeg - startDeg > 180 ? 1 : 0;
  return [
    `M ${o1.x.toFixed(2)} ${o1.y.toFixed(2)}`,
    `A ${outerR} ${outerR} 0 ${large} 1 ${o2.x.toFixed(2)} ${o2.y.toFixed(2)}`,
    `L ${i2.x.toFixed(2)} ${i2.y.toFixed(2)}`,
    `A ${innerR} ${innerR} 0 ${large} 0 ${i1.x.toFixed(2)} ${i1.y.toFixed(2)}`,
    "Z",
  ].join(" ");
}
