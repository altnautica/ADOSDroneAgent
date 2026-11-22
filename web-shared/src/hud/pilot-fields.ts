/**
 * @module hud/pilot-fields
 * @description Smaller parts of the `pilot` kind, parsed apart from
 * `cockpit-frames.ts`: the horizontal speed cap (`speed_cap`, and its value
 * as the flight-state chip prints it), the FC's STATUSTEXT lines, the stick
 * limits and stick command (`profile_limits`, `stick`), the steady take-off
 * (`sequence.liftoff`) and the skill's goal (`goal_kind`, `view`).
 *
 * Dependency-free on purpose: Mission Control copies this file byte for byte.
 *
 * @license GPL-3.0-only
 */

type Obj = Record<string, unknown>;

function obj(v: unknown): Obj | null {
  return typeof v === "object" && v !== null && !Array.isArray(v) ? (v as Obj) : null;
}
function num(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}
function str(v: unknown): string | null {
  return typeof v === "string" ? v : null;
}
function oneOf<T extends string>(v: unknown, values: readonly T[]): T | null {
  return values.find((x) => x === v) ?? null;
}

export const LIFTOFF_PHASES = ["spool", "level", "find", "bracket", "settle", "commit", "climb", "done", "stopped"] as const;
export type LiftoffPhase = (typeof LIFTOFF_PHASES)[number];

/** A steady take-off on an ArduPilot multirotor: its phase, its status
 *  line ("Spinning up", "Take-off stopped"), the longer line where a surface
 *  has room ("Climbing to 3 m"; null when the agent sends none), whether it
 *  loads a learned trim or probes for one, and the tilt from rest. */
export interface LaunchLiftoff {
  phase: LiftoffPhase;
  text: string;
  detail: string | null;
  trim: "learned" | "probe" | null;
  tiltDeg: number | null;
}

/** `sequence.liftoff`, or null when absent or of a phase this surface does
 *  not know. */
export function parseLiftoff(v: unknown): LaunchLiftoff | null {
  const o = obj(v);
  const phase = o ? oneOf(o.phase, LIFTOFF_PHASES) : null;
  if (!o || !phase) return null;
  return {
    phase,
    text: str(o.text) ?? "",
    detail: str(o.detail) || null,
    trim: oneOf(o.trim, ["learned", "probe"] as const),
    tiltDeg: num(o.tilt_deg),
  };
}

/** The pilot's horizontal speed cap: why, and the limit in m/s. */
export interface SpeedCap {
  reason: string;
  vHMps: number;
}

/** `speed_cap` from the pilot frame, or null when absent or malformed. */
export function parseSpeedCap(raw: unknown): SpeedCap | null {
  const o = obj(raw);
  const reason = o ? str(o.reason) : null;
  const vHMps = o ? num(o.v_h_mps) : null;
  return reason !== null && vHMps !== null ? { reason, vHMps } : null;
}

/** The charge a low-battery return home needs (`battery_reserve`), airborne:
 *  `needPct` is the level the return fires at, `basis` `energy` when the
 *  estimate of the way home sets it, `floor` when the fixed floor does. */
export interface BatteryReserve {
  needPct: number;
  basis: "energy" | "floor";
  returnS: number;
}

/** `battery_reserve` from the pilot frame, or null when absent or malformed. */
export function parseBatteryReserve(raw: unknown): BatteryReserve | null {
  const o = obj(raw);
  if (!o) return null;
  const needPct = num(o.need_pct);
  const returnS = num(o.return_s);
  const basis = o.basis === "energy" || o.basis === "floor" ? o.basis : null;
  return needPct !== null && returnS !== null && basis !== null ? { needPct, basis, returnS } : null;
}

/** Leaves or a ceiling the pilot sees overhead along the way of a lease
 *  goal (`overhead_ahead`): the highest height over the ground the airframe
 *  passes under it at (`flyUnderAglM`, m) and how far along the way it lies
 *  (`aheadM`, horizontal m), for the lease `leaseId`. */
export interface OverheadAhead {
  leaseId: number;
  flyUnderAglM: number;
  aheadM: number;
}

/** `overhead_ahead` from the pilot frame, or null when absent, malformed or
 *  clear (the pilot sends null heights while nothing overhead lies ahead). */
export function parseOverheadAhead(raw: unknown): OverheadAhead | null {
  const o = obj(raw);
  if (!o) return null;
  const leaseId = num(o.lease_id);
  const flyUnderAglM = num(o.fly_under_agl_m);
  const aheadM = num(o.ahead_m);
  return leaseId !== null && flyUnderAglM !== null && aheadM !== null ? { leaseId, flyUnderAglM, aheadM } : null;
}

/** The world model the pilot sees obstacles with: a 3D lidar's voxel map, a
 *  depth camera's, learned depth from a camera, or a planar lidar's scan. */
export const SENSING_TIERS = ["lidar3d", "depth", "camera", "planar"] as const;
export type SensingTier = (typeof SENSING_TIERS)[number];

/** The pilot's `sensing`: its tier, the range it sees clear along its motion
 *  (m) and the horizontal speed that still stops inside that range (m/s). */
export interface Sensing {
  tier: SensingTier;
  clearRangeM: number;
  speedLimitMps: number;
}

/** `sensing` from the pilot frame, or null when absent (an older agent),
 *  malformed or of a tier this surface does not know. */
export function parseSensing(raw: unknown): Sensing | null {
  const o = obj(raw);
  if (!o) return null;
  const tier = SENSING_TIERS.find((t) => t === o.tier) ?? null;
  const clearRangeM = num(o.clear_range_m);
  const speedLimitMps = num(o.speed_limit_mps);
  return tier !== null && clearRangeM !== null && speedLimitMps !== null ? { tier, clearRangeM, speedLimitMps } : null;
}

/** The kind of goal a skill's lease sent the pilot (`goal_kind`): a
 *  `follow_intent` (the pilot picks the viewpoint), the skill's own ring
 *  point (`follow`), an orbit, a hold (`position`) or a search (`vantage`). */
export const GOAL_KINDS = ["follow_intent", "follow", "orbit", "position", "vantage"] as const;
export type GoalKind = (typeof GOAL_KINDS)[number];

/** `goal_kind` from the pilot frame, or null when absent (no skill goal, an
 *  older agent) or of a kind this surface does not know. */
export function parseGoalKind(raw: unknown): GoalKind | null {
  return GOAL_KINDS.find((k) => k === raw) ?? null;
}

/** The viewpoint the pilot's view planner picked for a `follow_intent`
 *  (`view`): bearing from the subject (degrees, north clockwise), distance
 *  (m), height (m) and the share of the subject's predicted positions it
 *  sees (0-1). */
export interface FollowView {
  bearingDeg: number;
  radiusM: number;
  heightM: number;
  visibility: number;
}

/** `view` from the pilot frame, or null when absent or malformed. */
export function parseFollowView(raw: unknown): FollowView | null {
  const o = obj(raw);
  if (!o) return null;
  const bearingDeg = num(o.bearing_deg);
  const radiusM = num(o.radius_m);
  const heightM = num(o.height_m);
  const visibility = num(o.visibility);
  return bearingDeg !== null && radiusM !== null && heightM !== null && visibility !== null
    ? { bearingDeg, radiusM, heightM, visibility }
    : null;
}

/** What a tracked mover is: a person, a road vehicle, or something the
 *  obstacle sensors see move without a camera class (kept clear as a person). */
export const MOVER_KINDS = ["person", "vehicle", "unknown"] as const;
export type MoverKind = (typeof MOVER_KINDS)[number];

/** One moving obstacle the pilot tracks (`movers`), in the FC's local NED
 *  frame at the frame's time: position (m), velocity (m/s), one-sigma
 *  uncertainty (m), where it was learnt of (`vision` or `scan`) and how long
 *  since it was last measured (ms). */
export interface Mover {
  id: number;
  kind: MoverKind;
  source: "vision" | "scan";
  ne: [number, number];
  velNe: [number, number];
  sigmaM: number;
  ageMs: number;
}

function maybePair(v: unknown): [number, number] | null {
  if (!Array.isArray(v) || v.length !== 2) return null;
  const [a, b] = [num(v[0]), num(v[1])];
  return a !== null && b !== null ? [a, b] : null;
}

/** `movers` from the pilot frame: every well-formed entry, an empty list when
 *  absent (an older agent, or nothing tracked). An unknown kind reads `unknown`. */
export function parseMovers(raw: unknown): Mover[] {
  if (!Array.isArray(raw)) return [];
  return raw.flatMap((r): Mover[] => {
    const o = obj(r);
    const id = o ? num(o.id) : null;
    const ne = o ? maybePair(o.ne) : null;
    const velNe = o ? maybePair(o.vel_ne) : null;
    if (!o || id === null || ne === null || velNe === null) return [];
    const kind = MOVER_KINDS.find((k) => k === o.kind) ?? "unknown";
    const source = o.source === "vision" ? "vision" : "scan";
    return [{ id, kind, source, ne, velNe, sigmaM: num(o.sigma_m) ?? 0, ageMs: num(o.age_ms) ?? 0 }];
  });
}

/** The cap in m/s with at most one decimal (1.5 → "1.5", 2 → "2"), the same
 *  on every surface. */
export function speedCapText(vHMps: number): string {
  return String(Math.round(vHMps * 10) / 10);
}

/** The pilot does not trust the horizontal position: `reason` is
 *  `flow_disagrees`, `extnav_lost` or `extnav_degraded`; `detail` Navigation's
 *  own reason when it named one. */
export interface PositionUnverified {
  reason: string;
  detail: string | null;
  flowMps: number | null;
  estimateMps: number | null;
}

/** `position_unverified` from the pilot frame, or null when absent (an older
 *  agent) or malformed. */
export function parsePositionUnverified(raw: unknown): PositionUnverified | null {
  const o = obj(raw);
  const reason = o ? str(o.reason) : null;
  if (!o || reason === null) return null;
  return { reason, detail: str(o.detail), flowMps: num(o.flow_mps), estimateMps: num(o.estimate_mps) };
}

/** One FC STATUSTEXT line; severity is MAVLink's (0 emergency … 4 warning). */
export interface FcStatusText {
  severity: number;
  text: string;
  ageMs: number;
}

/** `statustext` from the pilot frame: at most 8 well-formed lines. */
export function parseStatusText(raw: unknown): FcStatusText[] {
  if (!Array.isArray(raw)) return [];
  const out: FcStatusText[] = [];
  for (const r of raw.slice(0, 8)) {
    const o = obj(r);
    const text = o ? str(o.text) : null;
    const severity = o ? num(o.severity) : null;
    if (!o || text === null || severity === null) continue;
    out.push({ severity, text, ageMs: num(o.age_ms) ?? 0 });
  }
  return out;
}

/** The limits the operator's sticks fly at now (`profile_limits`): the
 *  profile's row held under the flight controller's and the vehicle's limits
 *  and the place's caps. Speeds m/s, yaw °/s, accelerations m/s² (yaw °/s²),
 *  jerk m/s³, expo 0..1, the stop distance from full speed and the obstacle
 *  margin in metres (0 from an agent that does not send it). */
export interface ProfileLimits {
  profile: "cine" | "normal" | "sport";
  vHMps: number;
  vUpMps: number;
  vDownMps: number;
  yawDps: number;
  aAccMps2: number;
  aBrkMps2: number;
  jerkMps3: number;
  aVMps2: number;
  yawDps2: number;
  expoH: number;
  expoV: number;
  expoYaw: number;
  stopM: number;
  marginM: number;
}

const PROFILES = ["cine", "normal", "sport"] as const;

/** `profile_limits` from the pilot frame, or null when absent (an older
 *  agent) or malformed. */
export function parseProfileLimits(raw: unknown): ProfileLimits | null {
  const o = obj(raw);
  if (!o) return null;
  const profile = PROFILES.find((p) => p === o.profile);
  const vHMps = num(o.v_h_mps);
  if (!profile || vHMps === null) return null;
  const n = (k: string) => num(o[k]) ?? 0;
  return {
    profile,
    vHMps,
    vUpMps: n("v_up_mps"),
    vDownMps: n("v_down_mps"),
    yawDps: n("yaw_dps"),
    aAccMps2: n("a_acc_mps2"),
    aBrkMps2: n("a_brk_mps2"),
    jerkMps3: n("jerk_mps3"),
    aVMps2: n("a_v_mps2"),
    yawDps2: n("yaw_dps2"),
    expoH: n("expo_h"),
    expoV: n("expo_v"),
    expoYaw: n("expo_yaw"),
    stopM: n("stop_m"),
    marginM: n("margin_m"),
  };
}

/** The operator's sticks on the pilot's latest stick tick (`stick`): the
 *  asked velocity after the centre threshold and expo, the shaped reference
 *  sent (north-east, vertical + up, yaw rate + clockwise), whether it brakes
 *  to a hold, and the age and sequence of the stick frame flown. */
export interface StickCommand {
  askNeMps: [number, number];
  askVzMps: number;
  askYawDps: number;
  vNeMps: [number, number];
  aNeMps2: [number, number];
  vzMps: number;
  yawRateDps: number;
  braking: boolean;
  inputAgeMs: number;
  inputSeq: number;
}

function pair(v: unknown): [number, number] {
  return Array.isArray(v) ? [num(v[0]) ?? 0, num(v[1]) ?? 0] : [0, 0];
}

/** `stick` from the pilot frame, or null while the sticks do not fly (or
 *  from an older agent). */
export function parseStickCommand(raw: unknown): StickCommand | null {
  const o = obj(raw);
  const inputAgeMs = o ? num(o.input_age_ms) : null;
  if (!o || inputAgeMs === null) return null;
  return {
    askNeMps: pair(o.ask_ne_mps),
    askVzMps: num(o.ask_vz_mps) ?? 0,
    askYawDps: num(o.ask_yaw_dps) ?? 0,
    vNeMps: pair(o.v_ne_mps),
    aNeMps2: pair(o.a_ne_mps2),
    vzMps: num(o.vz_mps) ?? 0,
    yawRateDps: num(o.yaw_rate_dps) ?? 0,
    braking: o.braking === true,
    inputAgeMs,
    inputSeq: num(o.input_seq) ?? 0,
  };
}
