// Stick conditioning, the same on every surface (C4): per-pad calibration
// (centre and extents of each raw axis, plus the axis map a non-standard pad
// needs), a radial deadzone, and the stick mode that turns two sticks into the
// pilot's `[roll, pitch, thr, yaw]`. The pilot applies the expo, so nothing
// here curves the response. Pure functions: the poller and the tests share
// them.

import {
  BUTTON_BITS,
  STANDARD_AXES,
  STICK_MODES,
  TRIGGER_PRESSED,
  type ControlId,
  type StickMode,
} from "@/lib/pad/ds4-map";

/** Which raw axis index carries each stick axis. */
export interface AxisMap {
  stick_l_x: number;
  stick_l_y: number;
  stick_r_x: number;
  stick_r_y: number;
}

export const STANDARD_AXIS_MAP: AxisMap = { ...STANDARD_AXES };

/** One pad's calibration, keyed by raw axis index. */
export interface PadCalibration {
  centre: number[];
  min: number[];
  max: number[];
  /** Set for a pad the browser does not report in the standard layout. */
  axisMap: AxisMap | null;
}

/** Stick position, screen convention: x + right, y + down, each in [-1, 1]. */
export type Stick = readonly [number, number];

export interface Sticks {
  l: Stick;
  r: Stick;
}

export const CENTRED_STICKS: Sticks = { l: [0, 0], r: [0, 0] };

function clamp1(v: number): number {
  return v > 1 ? 1 : v < -1 ? -1 : v;
}

/** One raw axis through its calibration: centre to 0, each half scaled by its
 *  own extent so a stick that reaches 0.92 one way and 1.0 the other still
 *  spans [-1, 1]. Uncalibrated axes pass through, clamped. */
export function calibrateAxis(raw: number, index: number, cal: PadCalibration | null): number {
  if (!Number.isFinite(raw)) return 0;
  if (!cal) return clamp1(raw);
  const c = cal.centre[index] ?? 0;
  const lo = cal.min[index] ?? -1;
  const hi = cal.max[index] ?? 1;
  const v = raw - c;
  if (v >= 0) return hi - c > 1e-3 ? clamp1(v / (hi - c)) : 0;
  return c - lo > 1e-3 ? clamp1(v / (c - lo)) : 0;
}

/** A radial deadzone: inside `dz` reads centred; outside, the magnitude is
 *  rescaled from `dz..1` to `0..1` with the direction kept, so there is no
 *  step at the edge and diagonals are not squared off. */
export function radialDeadzone(x: number, y: number, dz: number): Stick {
  const m = Math.hypot(x, y);
  if (m <= dz || m === 0) return [0, 0];
  const scaled = Math.min(1, (m - dz) / (1 - dz));
  return [(x / m) * scaled, (y / m) * scaled];
}

/** Raw axes → both conditioned sticks. */
export function conditionSticks(
  axes: readonly number[],
  cal: PadCalibration | null,
  deadzone: number,
): Sticks {
  const map = cal?.axisMap ?? STANDARD_AXIS_MAP;
  const read = (i: number) => calibrateAxis(axes[i] ?? 0, i, cal);
  return {
    l: radialDeadzone(read(map.stick_l_x), read(map.stick_l_y), deadzone),
    r: radialDeadzone(read(map.stick_r_x), read(map.stick_r_y), deadzone),
  };
}

/** Sticks → the pilot's `[roll, pitch, thr, yaw]` per the stick mode. Mode 2
 *  puts throttle and yaw on the left stick; mode 1 swaps the two Y axes. The
 *  signs follow C1: pitch + forward, thr + climb, yaw + clockwise, roll +
 *  right (stick Y up is negative on the wire, hence the spec's `invert`). */
export function pilotAxes(sticks: Sticks, mode: StickMode): [number, number, number, number] {
  const spec = STICK_MODES[mode];
  const pick = (k: keyof typeof spec) => {
    const a = spec[k];
    const s = a.stick === "stick_l" ? sticks.l : sticks.r;
    const v = a.axis === "x" ? s[0] : s[1];
    return a.invert ? -v || 0 : v;
  };
  return [pick("roll"), pick("pitch"), pick("throttle"), pick("yaw")];
}

/** The stream's button bitfield: one bit per control at its standard index;
 *  L2/R2 count as pressed from the spec's trigger threshold. */
export function buttonsBitfield(pressed: readonly boolean[], values: readonly number[]): number {
  let bits = 0;
  for (const [control, bit] of Object.entries(BUTTON_BITS) as [ControlId, number][]) {
    const isTrigger = control === "l2" || control === "r2";
    const on = isTrigger ? (values[bit] ?? 0) >= TRIGGER_PRESSED : pressed[bit] === true;
    if (on) bits |= 1 << bit;
  }
  return bits >>> 0;
}

/** Calibration recording: fold one raw sample into the running extents. The
 *  centre is the first (released) sample; the operator then sweeps both
 *  sticks to every edge. */
export function foldCalibration(cal: PadCalibration | null, axes: readonly number[]): PadCalibration {
  if (!cal) {
    return { centre: [...axes], min: [...axes], max: [...axes], axisMap: null };
  }
  return {
    centre: cal.centre,
    min: axes.map((v, i) => Math.min(cal.min[i] ?? v, v)),
    max: axes.map((v, i) => Math.max(cal.max[i] ?? v, v)),
    axisMap: cal.axisMap,
  };
}

/** Re-centre a recorded calibration on the released sample. */
export function recentre(cal: PadCalibration, axes: readonly number[]): PadCalibration {
  return { ...cal, centre: [...axes] };
}

/** Whether a recording swept far enough to use: every mapped axis moved at
 *  least half its travel each way. */
export function calibrationComplete(cal: PadCalibration): boolean {
  const map = cal.axisMap ?? STANDARD_AXIS_MAP;
  return [map.stick_l_x, map.stick_l_y, map.stick_r_x, map.stick_r_y].every((i) => {
    const c = cal.centre[i] ?? 0;
    return (cal.max[i] ?? c) - c >= 0.5 && c - (cal.min[i] ?? c) >= 0.5;
  });
}

/** For a non-standard pad: the raw axis that moved furthest from its centre
 *  (the operator is asked to push one stick one way at a time). */
export function dominantAxis(axes: readonly number[], centre: readonly number[], exclude: readonly number[]): number | null {
  let best: number | null = null;
  let bestDelta = 0.5;
  axes.forEach((v, i) => {
    if (exclude.includes(i)) return;
    const d = Math.abs(v - (centre[i] ?? 0));
    if (d > bestDelta) {
      bestDelta = d;
      best = i;
    }
  });
  return best;
}
