// Pure geometry for the Feed HUD canvas: pitch ladder rungs, roll scale,
// tape ticks, heading tape, the flight-path marker and instrument staleness.
// Angles are degrees; screen y grows downward.

/** An instrument whose source is older than this dims and reads a dash. */
export const STALE_MS = 2000;

/** Roll scale ticks (degrees either side of wings-level). */
export const ROLL_TICKS: readonly number[] = [-60, -45, -30, -20, -10, 10, 20, 30, 45, 60];

/** Whether the instruments must dim: no live telemetry, or the last live
 *  sample is older than `STALE_MS`. */
export function isInstrumentStale(nowMs: number, lastLiveAt: number | null, live: boolean): boolean {
  return !live || lastLiveAt === null || nowMs - lastLiveAt > STALE_MS;
}

export interface Rung {
  deg: number;
  /** Offset from the boresight along the rolled vertical, px (down positive). */
  y: number;
  /** Labelled rung (every 10°); the rest are 5° minor rungs. */
  label: boolean;
}

/** The pitch-ladder rungs visible around `pitchDeg`: one every 5° within
 *  ±`halfRangeDeg`, labelled every 10°. The horizon (0°) is drawn separately. */
export function pitchRungs(pitchDeg: number, pxPerDeg: number, halfRangeDeg: number): Rung[] {
  const out: Rung[] = [];
  const first = Math.ceil((pitchDeg - halfRangeDeg) / 5) * 5;
  for (let d = first; d <= pitchDeg + halfRangeDeg; d += 5) {
    if (d === 0 || d < -90 || d > 90) continue;
    out.push({ deg: d, y: (pitchDeg - d) * pxPerDeg, label: d % 10 === 0 });
  }
  return out;
}

/** Multiples of `step` within `value ± halfSpan` (a scrolling tape). */
export function tapeTicks(value: number, halfSpan: number, step: number): number[] {
  const out: number[] = [];
  for (let v = Math.ceil((value - halfSpan) / step) * step; v <= value + halfSpan; v += step) {
    out.push(Math.round(v * 1000) / 1000);
  }
  return out;
}

/** Signed shortest angle from `from` to `to`, in (-180, 180]. */
export function angleDelta(to: number, from: number): number {
  const d = (((to - from) % 360) + 540) % 360 - 180;
  return d === -180 ? 180 : d;
}

export interface HeadingTick {
  /** Compass degrees 0..355. */
  deg: number;
  /** Offset from the current heading, degrees. */
  delta: number;
}

/** Heading-tape ticks every `step` degrees within `heading ± halfSpan`. */
export function headingTicks(heading: number, halfSpan: number, step = 5): HeadingTick[] {
  const out: HeadingTick[] = [];
  const first = Math.ceil((heading - halfSpan) / step) * step;
  for (let d = first; d <= heading + halfSpan; d += step) {
    out.push({ deg: ((d % 360) + 360) % 360, delta: d - heading });
  }
  return out;
}

export interface FlightPath {
  /** Track minus heading: how far the velocity points off the nose. */
  driftDeg: number;
  /** Flight-path angle above the horizon. */
  fpaDeg: number;
}

/** The flight-path marker from the NED velocity, or null when the vehicle is
 *  too slow for a meaningful direction (or the velocity is absent). */
export function flightPath(
  vx: number | null | undefined,
  vy: number | null | undefined,
  vz: number | null | undefined,
  headingDeg: number | null | undefined,
): FlightPath | null {
  if (![vx, vy, vz, headingDeg].every((v) => typeof v === "number" && Number.isFinite(v))) return null;
  const gs = Math.hypot(vx as number, vy as number);
  if (gs < 1) return null;
  const track = (Math.atan2(vy as number, vx as number) * 180) / Math.PI;
  return {
    driftDeg: angleDelta(track, headingDeg as number),
    fpaDeg: (Math.atan2(-(vz as number), gs) * 180) / Math.PI,
  };
}
