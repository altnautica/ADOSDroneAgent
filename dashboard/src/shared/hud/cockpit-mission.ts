/**
 * @module hud/cockpit-mission
 * @description The mission parts of the cockpit v2 `pilot` kind, parsed apart
 * from `cockpit-frames.ts`: the agent-computed mission state with its target
 * and coverage gaps, the mission stall, the mission detour and routed return
 * with the way round a map previews, and the place class and route check.
 * `cockpit-frames.ts` re-exports everything here.
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
  return typeof v === "string" && (values as readonly string[]).includes(v) ? (v as T) : null;
}
function pt3(v: unknown): [number, number, number] | null {
  if (!Array.isArray(v) || v.length < 3) return null;
  const a = num(v[0]);
  const b = num(v[1]);
  const c = num(v[2]);
  return a === null || b === null || c === null ? null : [a, b, c];
}
function nums(v: unknown): number[] {
  return Array.isArray(v) ? v.filter((s): s is number => num(s) !== null) : [];
}

// ── mission state ────────────────────────────────────────────────────

export const MISSION_STATES = ["none", "loaded", "active", "paused", "complete", "returning"] as const;
export type MissionStateName = (typeof MISSION_STATES)[number];

/** The mission item the FC flies now (`mission.target`). `ned` is the item
 *  position in the FC's local NED frame, null for an item without one. */
export interface MissionTarget {
  seq: number;
  cmd: number | null;
  ned: [number, number, number] | null;
  radiusM: number | null;
}

/** One skipped mission item (`mission.gaps[]`): a coverage gap to re-fly. */
export interface MissionGap {
  seq: number;
  /** `stall_timeout` (skipped after the countdown), `stall_operator`, or
   *  proven unreachable at once: `unreachable_inside_obstacle`,
   *  `unreachable_no_path` (`gapReasonText` words each). */
  reason: string | null;
  atMs: number | null;
}

/** The agent-computed mission state. `seq`/`total` follow MISSION_CURRENT. */
export interface MissionState {
  state: MissionStateName | null;
  seq: number | null;
  total: number | null;
  itemReached: number | null;
  id: number | null;
  source: "mission_state" | "fallback" | null;
  ageMs: number | null;
  landingSeq: boolean;
  uploadsSeen: number | null;
  target: MissionTarget | null;
  /** Items skipped this mission, oldest first; empty while none. */
  gaps: MissionGap[];
}

function parseMissionTarget(raw: unknown): MissionTarget | null {
  const t = obj(raw);
  const seq = t ? num(t.seq) : null;
  if (!t || seq === null) return null;
  return { seq, cmd: num(t.cmd), ned: pt3(t.ned), radiusM: num(t.radius_m) };
}

function parseMissionGaps(raw: unknown): MissionGap[] {
  if (!Array.isArray(raw)) return [];
  const out: MissionGap[] = [];
  for (const g of raw) {
    const o = obj(g);
    const seq = o ? num(o.seq) : null;
    if (!o || seq === null) continue;
    out.push({ seq, reason: str(o.reason), atMs: num(o.at_ms) });
  }
  return out;
}

/** Accepts the full `state.v5` object (`state`, `seq`, `total`, `landing_seq`)
 *  and the short `pilot` form (`current_seq`, `total`). */
export function parseMission(raw: unknown): MissionState | null {
  const m = obj(raw);
  if (!m) return null;
  return {
    state: oneOf(m.state, MISSION_STATES),
    seq: num(m.seq) ?? num(m.current_seq),
    total: num(m.total),
    itemReached: num(m.item_reached),
    id: num(m.id),
    source: oneOf(m.source, ["mission_state", "fallback"] as const),
    ageMs: num(m.age_ms),
    landingSeq: m.landing_seq === true,
    uploadsSeen: num(m.uploads_seen),
    target: parseMissionTarget(m.target),
    gaps: parseMissionGaps(m.gaps),
  };
}

/** A mission or return that stopped making progress (`mission_stall`): the
 *  vehicle holds and the operator chooses. Persistent while it lasts. */
export interface MissionStall {
  kind: "mission" | "return";
  /** The stalled mission item; null for a return. */
  seq: number | null;
  /** The item's label as the plan shows it; null for a return. */
  wpLabel: string | null;
  /** Wall-clock epoch ms the stall was raised. */
  sinceMs: number | null;
  /** Seconds to the automatic action (the skip for a mission, the landing
   *  for a return); null: no countdown, the vehicle holds for the operator. */
  skipInS: number | null;
  /** Seconds until the vehicle flies SmartRTL home on its own: a mission
   *  held for the operator in a confined place (no skip left, `skipInS`
   *  null) that no operator has answered; null otherwise. */
  returnInS: number | null;
  /** The no-progress window that raised it, seconds. */
  windowS: number | null;
}

export function parseMissionStall(raw: unknown): MissionStall | null {
  const s = obj(raw);
  const kind = s ? oneOf(s.kind, ["mission", "return"] as const) : null;
  if (!s || !kind) return null;
  return {
    kind,
    seq: num(s.seq),
    wpLabel: str(s.wp_label),
    sinceMs: num(s.since_ms),
    skipInS: num(s.skip_in_s),
    returnInS: num(s.return_in_s),
    windowS: num(s.window_s),
  };
}

// ── routing round (avoid mode bypass) ───────────────────────────────────────

/** A detour round a blocked mission leg (`mission_detour`): the FC is in
 *  GUIDED while the pilot flies its planner to the leg's next free point,
 *  then it resumes AUTO at mission item `seq`. */
export interface MissionDetour {
  seq: number;
  /** The leg's next free point, local NED metres. */
  targetNed: [number, number, number];
  /** Wall-clock epoch ms the detour began. */
  sinceMs: number | null;
}

export function parseMissionDetour(raw: unknown): MissionDetour | null {
  const d = obj(raw);
  const seq = d ? num(d.seq) : null;
  const targetNed = d ? pt3(d.target_ned) : null;
  if (!d || seq === null || !targetNed) return null;
  return { seq, targetNed, sinceMs: num(d.since_ms) };
}

/** A return home the pilot flies itself round what blocked the FC's own
 *  (`return_route`); the low-battery reserve counts `remainingM`. */
export interface ReturnRoute {
  /** The planned way still to fly home, horizontal metres. */
  remainingM: number;
  /** The straight line home, horizontal metres. */
  straightM: number;
  sinceMs: number | null;
}

export function parseReturnRoute(raw: unknown): ReturnRoute | null {
  const r = obj(raw);
  const remainingM = r ? num(r.remaining_m) : null;
  const straightM = r ? num(r.straight_m) : null;
  if (!r || remainingM === null || straightM === null) return null;
  return { remainingM, straightM, sinceMs: num(r.since_ms) };
}

/** The way round a surface previews on its map while the pilot routes round
 *  (a mission detour or a routed return): the pilot's planned path
 *  (`plan_ned`, north/east metres), and the detour's free point it resumes
 *  the mission at. Null while the pilot routes round nothing or plans no
 *  path and names no point. */
export interface RouteRoundPreview {
  kind: "detour" | "return";
  pathNe: [number, number][];
  targetNe: [number, number] | null;
}

export function routeRoundPreview(p: {
  missionDetour: MissionDetour | null;
  returnRoute: ReturnRoute | null;
  planNed: [number, number][];
}): RouteRoundPreview | null {
  const kind = p.missionDetour ? "detour" : p.returnRoute ? "return" : null;
  if (!kind) return null;
  const t = p.missionDetour?.targetNed ?? null;
  const targetNe: [number, number] | null = t ? [t[0], t[1]] : null;
  if (p.planNed.length < 2 && !targetNe) return null;
  return { kind, pathNe: p.planNed.length >= 2 ? p.planNed : [], targetNe };
}

// ── place class and route check ─────────────────────────────────────────────

/** The operator's place class: `confined` flies tighter to walls and slower
 *  near the limit. */
export const PLACE_CLASSES = ["open", "confined"] as const;
export type PlaceClass = (typeof PLACE_CLASSES)[number];

/** The tightest checked point of the route. */
export interface RoutePinch {
  /** The leg runs from mission item `fromSeq` to `toSeq`. */
  fromSeq: number;
  toSeq: number;
  /** Local NED of the point, metres. */
  ned: [number, number, number];
  clearanceM: number;
  requiredM: number;
}

/** The pilot's check of the loaded mission against its occupancy grid, in
 *  every place class while the FC holds a mission with at least one leg. */
export interface RouteCheck {
  state: "feasible" | "unverified" | "infeasible";
  /** Clearance the route must keep: the margin plus the pose uncertainty. */
  requiredM: number | null;
  marginM: number | null;
  /** Live pose uncertainty; null while unknown. */
  sigmaM: number | null;
  pinch: RoutePinch | null;
  /** `to_seq` of the legs that cross cells not yet observed. */
  unverifiedLegs: number[];
  /** Mission items the check cannot place (no position): the legs to and
   *  from them are not checked. */
  unchecked: number[];
  /** The mission is longer than the route the check holds: its end is not
   *  checked. */
  truncated: boolean;
  /** The mission feed may not hold the FC's mission (not downloaded yet,
   *  or out of step with the FC's count). */
  stale: boolean;
}

function parseRoutePinch(raw: unknown): RoutePinch | null {
  const p = obj(raw);
  if (!p) return null;
  const fromSeq = num(p.from_seq);
  const toSeq = num(p.to_seq);
  const ned = pt3(p.ned);
  const clearanceM = num(p.clearance_m);
  const requiredM = num(p.required_m);
  if (fromSeq === null || toSeq === null || !ned || clearanceM === null || requiredM === null) return null;
  return { fromSeq, toSeq, ned, clearanceM, requiredM };
}

export function parseRouteCheck(raw: unknown): RouteCheck | null {
  const r = obj(raw);
  const state = r ? oneOf(r.state, ["feasible", "unverified", "infeasible"] as const) : null;
  if (!r || !state) return null;
  return {
    state,
    requiredM: num(r.required_m),
    marginM: num(r.margin_m),
    sigmaM: num(r.sigma_m),
    pinch: parseRoutePinch(r.pinch),
    unverifiedLegs: nums(r.unverified_legs),
    unchecked: nums(r.unchecked),
    truncated: r.truncated === true,
    stale: r.stale === true,
  };
}
