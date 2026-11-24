/**
 * @module hud/cockpit-frames
 * @description Typed shapes and boundary parsers for the cockpit v2 nav-stream
 * kinds the agent computes for every surface: `pilot` (the command authority's
 * state), `obstacles` (radar sectors and per-camera nearness grids), `ar`
 * (projected route and motion items per camera) and `depth` (the model depth
 * grid). Payloads arrive snake case; the parsers narrow `unknown` into the
 * camelCase shapes the layer renderers read and return null for anything
 * malformed instead of guessing.
 *
 * Grids stay base64 strings here: the renderer decodes them into a reused
 * byte buffer (`decodeBase64Into`), so a frame costs no fresh allocation.
 *
 * Dependency-free on purpose apart from its siblings `pilot-fields.ts`,
 * `cockpit-grids.ts` (the `obstacles`/`depth` grids), `cockpit-mission.ts`
 * (mission state, stall, place class and route check),
 * `cockpit-surfaces.ts` (landing-zone candidates) and `cockpit-ar.ts` (the
 * `ar` items), all re-exported here: Mission Control copies them all byte
 * for byte.
 *
 * @license GPL-3.0-only
 */

import {
  parseBatteryReserve,
  parseFollowView,
  parseGoalKind,
  parseMovers,
  parseLiftoff,
  parseOverheadAhead,
  parsePositionUnverified,
  parseProfileLimits,
  parseSensing,
  parseSpeedCap,
  parseStatusText,
  parseStickCommand,
  type BatteryReserve,
  type FcStatusText,
  type FollowView,
  type GoalKind,
  type LaunchLiftoff,
  type Mover,
  type OverheadAhead,
  type PositionUnverified,
  type ProfileLimits,
  type Sensing,
  type SpeedCap,
  type StickCommand,
} from "./pilot-fields";
import { PLACE_CLASSES, parseMission, parseMissionDetour, parseMissionStall, parseReturnRoute, parseRouteCheck } from "./cockpit-mission";
import type { MissionDetour, MissionStall, MissionState, PlaceClass, ReturnRoute, RouteCheck } from "./cockpit-mission";
import { parseSimClock, type SimClock } from "./sim-clock";
import { AVOID_STATES, type AvoidState } from "./cockpit-ar";

export type {
  BatteryReserve,
  FcStatusText,
  FollowView,
  GoalKind,
  OverheadAhead,
  PositionUnverified,
  ProfileLimits,
  Sensing,
  SpeedCap,
  StickCommand,
} from "./pilot-fields";
export {
  decodeBase64Into,
  decodeGridCode,
  gridCodeFor,
  parseDepthFrame,
  parseObstaclesFrame,
  type CameraGrid,
  type DepthFrame,
  type ObstaclesFrame,
} from "./cockpit-grids";
export { MISSION_STATES, PLACE_CLASSES, parseMission, parseMissionDetour, parseMissionStall, parseReturnRoute, parseRouteCheck, routeRoundPreview } from "./cockpit-mission";
export type {
  MissionDetour,
  MissionGap,
  MissionStall,
  MissionState,
  MissionStateName,
  MissionTarget,
  PlaceClass,
  ReturnRoute,
  RouteCheck,
  RoutePinch,
  RouteRoundPreview,
} from "./cockpit-mission";
export { parseSurfacesFrame, type SurfaceZone, type SurfacesFrame } from "./cockpit-surfaces";
export { parseArFrame, type ArCamera, type ArFrame, type ArItem } from "./cockpit-ar";

// ── small readers ─────────────────────────────────────────────────────────

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
function pt2(v: unknown): [number, number] | null {
  if (!Array.isArray(v) || v.length < 2) return null;
  const a = num(v[0]);
  const b = num(v[1]);
  return a === null || b === null ? null : [a, b];
}
function pt3(v: unknown): [number, number, number] | null {
  if (!Array.isArray(v) || v.length < 3) return null;
  const a = num(v[0]);
  const b = num(v[1]);
  const c = num(v[2]);
  return a === null || b === null || c === null ? null : [a, b, c];
}
function pts2(v: unknown, max = Infinity): [number, number][] {
  if (!Array.isArray(v)) return [];
  const out: [number, number][] = [];
  for (const p of v) {
    if (out.length >= max) break;
    const q = pt2(p);
    if (q) out.push(q);
  }
  return out;
}

// ── pilot (nav kind `pilot` with the flow fields) ────────────────────────────────────

export const PILOT_OWNERS = ["idle", "pilot", "skill", "goto", "swarm", "mission_fc", "fc_mode", "tuning"] as const;
export type PilotOwner = (typeof PILOT_OWNERS)[number];
export type { AvoidState } from "./cockpit-ar";
export type VehicleClass = "multirotor" | "fixedwing" | "vtol";

/** One flight-controller parameter a readiness item compares: the FC's
 *  value (null while the FC does not report it) and what the rule wants, as
 *  text ("1", "5 or 7"). */
export interface ParamDiff {
  name: string;
  have: number | null;
  want: string;
}

/** What fixes a failing readiness item: run the setup engine's ADOS profile
 *  step, open Aircraft setup at a step, open a settings section, or open the
 *  parameter editor at these names. */
export type ReadinessFix =
  | { kind: "apply_profile" }
  | { kind: "setup_step"; step: string }
  | { kind: "settings"; section: string }
  | { kind: "parameters"; names: string[] };

export interface ReadinessItem {
  id: string;
  ok: boolean;
  severity: "block" | "warn";
  msg: string;
  /** The parameter rows behind the item; absent when it names none. */
  params?: ParamDiff[];
  /** The fix the agent offers; absent when none. */
  fix?: ReadinessFix;
}

/** A readiness item's `params`, keeping only well-formed rows; null when
 *  absent or empty. */
export function parseParamDiffs(v: unknown): ParamDiff[] | null {
  if (!Array.isArray(v)) return null;
  const out: ParamDiff[] = [];
  for (const r of v) {
    const o = obj(r);
    const name = o ? str(o.name) : null;
    if (!o || name === null) continue;
    out.push({ name, have: num(o.have), want: str(o.want) ?? (num(o.want) !== null ? String(o.want) : "") });
  }
  return out.length > 0 ? out : null;
}

/** A readiness item's `fix`; null when absent or of an unknown kind. */
export function parseReadinessFix(v: unknown): ReadinessFix | null {
  const o = obj(v);
  if (!o) return null;
  switch (o.kind) {
    case "apply_profile":
      return { kind: "apply_profile" };
    case "setup_step": {
      const step = str(o.step);
      return step === null ? null : { kind: "setup_step", step };
    }
    case "settings": {
      const section = str(o.section);
      return section === null ? null : { kind: "settings", section };
    }
    case "parameters": {
      const names = Array.isArray(o.names) ? o.names.filter((n): n is string => typeof n === "string") : [];
      return names.length > 0 ? { kind: "parameters", names } : null;
    }
    default:
      return null;
  }
}

export interface LaunchSequence {
  step: "checks" | "arm" | "takeoff" | "after" | "done";
  after: string | null;
  error: string | null;
  /** Present while the take-off is a steady take-off; absent from an agent
   *  without one. */
  liftoff?: LaunchLiftoff | null;
}

export interface PilotState {
  owner: PilotOwner;
  ownerLabel: string | null;
  skill: { pluginId: string; skillId: string; label: string } | null;
  controlMode: "ados" | "fc_manual" | "elrs" | null;
  profile: "cine" | "normal" | "sport" | null;
  avoid: "brake" | "bypass" | "off" | null;
  fcMode: number | null;
  fcModeName: string | null;
  /** Null while the pilot has no flight-controller link: the arm state is
   *  unknown, and every surface treats the drone as possibly airborne. */
  armed: boolean | null;
  landed: boolean | null;
  vehicleClass: VehicleClass | null;
  inputSrc: string | null;
  inputAgeMs: number | null;
  holdReason: string | null;
  avoidState: AvoidState | null;
  minClearanceM: number | null;
  goalNed: [number, number, number] | null;
  planNed: [number, number][];
  setpointNed: [number, number, number] | null;
  readiness: ReadinessItem[];
  linkLossInS: number | null;
  /** What the pilot does when the countdown ends; null from an agent that
   *  does not report it. */
  linkLossAction: "rth" | "rtl" | "hold" | "loiter" | "land" | null;
  /** The link-loss window in seconds; null from an agent that does not
   *  report it. */
  linkLossS: number | null;
  /** The FC is flying a VTOL transition (either direction). */
  vtolTransitioning: boolean;
  mission: MissionState | null;
  inputSession: { src: string; label: string; session: number | null } | null;
  gate: { pending: boolean; progress: number } | null;
  sequence: LaunchSequence | null;
  banner: { textKey: string; args: Record<string, unknown>; untilMs: number | null } | null;
  rthCountdownS: number | null;
  /** A mission or return that stopped making progress; null while none. */
  missionStall: MissionStall | null;
  /** A detour round a blocked mission leg in flight; null while none. */
  missionDetour: MissionDetour | null;
  /** A return home the pilot routes round a blocked one; null while none. */
  returnRoute: ReturnRoute | null;
  /** Pilot actions this agent accepts from a surface (e.g. `goto`). */
  surfaceOps: string[];
  /** Height above ground, from a fresh downward rangefinder or the EKF
   *  height above home. */
  aglM: number | null;
  aglSource: "rangefinder" | "home" | null;
  /** FC STATUSTEXT at WARNING or worse from the last 30 s, newest first. */
  statustext: FcStatusText[];
  /** The setup step a tuning flight is on (owner `tuning`), or null. */
  setupStep: string | null;
  /** The FC flies an RTL or land of its own (a failsafe, not an operator's
   *  mode change). The agent omits `fc_failsafe` while it is false. */
  fcFailsafe: boolean;
  /** Why `fcFailsafe`: `rc_or_failsafe` (RC input was present, so an RC
   *  switch is possible) or `fc`; null while not in a failsafe. */
  failsafeCause: string | null;
  /** The setup-strings key (`setup.blocker.aborted_<reason>`) of the air
   *  wizard's last abort that was not the operator's while the engine still
   *  shows it, or null. */
  setupAbort: string | null;
  /** The pilot's own low-battery action: `land` (it may fly without GNSS)
   *  or `rth`, while its countdown runs (`rthCountdownS`) and after it
   *  fired; null while none. */
  lowBatteryAction: "land" | "rth" | null;
  /** The charge a low-battery return home needs, airborne; null on the
   *  ground. */
  batteryReserve: BatteryReserve | null;
  /** A horizontal speed cap the pilot applies to every setpoint it flies
   *  (`gnss-denied` while the EKF horizontal source is not GNSS,
   *  `confined-near` near the clearance limit in a confined place), or null. */
  speedCap: SpeedCap | null;
  /** The operator's place class; `open` from an agent that omits it. */
  placeClass: PlaceClass;
  /** `absent` while the drone is declared to fly with no RC transmitter;
   *  `present` from an agent that omits it. */
  rcReceiver: "absent" | "present";
  /** The FC mission's route check (any place class); else null. */
  routeCheck: RouteCheck | null;
  /** The raw `sensors` block (parse with sensor-health); null when absent. */
  sensors: unknown;
  /** The pilot does not trust the horizontal position (an older agent sends
   *  none), or null. */
  positionUnverified: PositionUnverified | null;
  /** The position's one-sigma uncertainty (m) while it is over a metre yet
   *  within the pilot's limit: flown on, slowed (`pose_uncertainty_m`; an
   *  older agent sends none), or null. */
  poseUncertaintyM: number | null;
  /** Leaves or a ceiling the pilot sees overhead along the lease goal's way
   *  (`overhead_ahead`), or null while nothing lies ahead. */
  overheadAhead: OverheadAhead | null;
  /** What the pilot sees obstacles with and the speed that stops inside the
   *  range it sees clear (`sensing`), or null (an older agent). */
  sensing: Sensing | null;
  /** The kind of goal the skill's lease sent (`goal_kind`), or null while a
   *  skill flies none (or an older agent). */
  goalKind: GoalKind | null;
  /** The viewpoint the view planner picked for a `follow_intent` (`view`),
   *  or null. */
  view: FollowView | null;
  /** The moving obstacles the pilot tracks (`movers`); empty when none. */
  movers: Mover[];
  /** The limits the operator's sticks fly at now, or null (an older agent). */
  profileLimits: ProfileLimits | null;
  /** The operator's sticks on the latest stick tick, while they own the
   *  vehicle; else null. */
  stick: StickCommand | null;
  /** The limits that cut the asked velocity since the previous frame
   *  (`command.limits`, first bound first); empty while it flies none. */
  commandLimits: string[];
  /** The simulator's clock while the pilot follows one (the "SIM ×N"
   *  badge); null on wall time and from an older agent. */
  simClock: SimClock | null;
}

const SEQ_STEPS = ["checks", "arm", "takeoff", "after", "done"] as const;

export function parsePilotState(raw: unknown): PilotState | null {
  const p = obj(raw);
  if (!p) return null;
  const owner = oneOf(p.owner, PILOT_OWNERS);
  if (!owner) return null;
  const sk = obj(p.skill);
  const skill =
    sk && str(sk.plugin_id) !== null && str(sk.skill_id) !== null
      ? { pluginId: sk.plugin_id as string, skillId: sk.skill_id as string, label: str(sk.label) ?? (sk.skill_id as string) }
      : null;
  const readiness: ReadinessItem[] = [];
  if (Array.isArray(p.readiness)) {
    for (const r of p.readiness) {
      const o = obj(r);
      const id = o ? str(o.id) : null;
      if (!o || id === null) continue;
      const item: ReadinessItem = {
        id,
        ok: o.ok === true,
        severity: o.severity === "warn" ? "warn" : "block",
        msg: str(o.msg) ?? "",
      };
      const params = parseParamDiffs(o.params);
      if (params) item.params = params;
      const fix = parseReadinessFix(o.fix);
      if (fix) item.fix = fix;
      readiness.push(item);
    }
  }
  const is = obj(p.input_session);
  const gate = obj(p.gate);
  const seq = obj(p.sequence);
  const seqStep = seq ? oneOf(seq.step, SEQ_STEPS) : null;
  const banner = obj(p.banner);
  return {
    owner,
    ownerLabel: str(p.owner_label),
    skill,
    controlMode: oneOf(p.control_mode, ["ados", "fc_manual", "elrs"] as const),
    profile: oneOf(p.profile, ["cine", "normal", "sport"] as const),
    avoid: oneOf(p.avoid, ["brake", "bypass", "off"] as const),
    fcMode: num(p.fc_mode),
    fcModeName: str(p.fc_mode_name),
    armed: typeof p.armed === "boolean" ? p.armed : null,
    landed: typeof p.landed === "boolean" ? p.landed : null,
    vehicleClass: oneOf(p.vehicle_class, ["multirotor", "fixedwing", "vtol"] as const),
    inputSrc: str(p.input_src),
    inputAgeMs: num(p.input_age_ms),
    holdReason: str(p.hold_reason),
    avoidState: oneOf(p.avoid_state, AVOID_STATES),
    minClearanceM: num(p.min_clearance_m),
    goalNed: pt3(p.goal_ned),
    planNed: pts2(p.plan_ned, 16),
    setpointNed: pt3(p.setpoint_ned),
    readiness,
    linkLossInS: num(p.link_loss_in_s),
    linkLossAction: oneOf(p.link_loss_action, ["rth", "rtl", "hold", "loiter", "land"] as const),
    linkLossS: num(p.link_loss_s),
    vtolTransitioning: p.vtol_transitioning === true,
    mission: parseMission(p.mission),
    inputSession: is && str(is.src) !== null ? { src: is.src as string, label: str(is.label) ?? (is.src as string), session: num(is.session) } : null,
    gate: gate ? { pending: gate.pending === true, progress: Math.min(1, Math.max(0, num(gate.progress) ?? 0)) } : null,
    sequence: seq && seqStep ? { step: seqStep, after: str(seq.after), error: str(seq.error), liftoff: parseLiftoff(seq.liftoff) } : null,
    banner:
      banner && str(banner.text_key) !== null
        ? { textKey: banner.text_key as string, args: obj(banner.args) ?? {}, untilMs: num(banner.until_ms) }
        : null,
    rthCountdownS: num(p.rth_countdown_s),
    missionStall: parseMissionStall(p.mission_stall),
    missionDetour: parseMissionDetour(p.mission_detour),
    returnRoute: parseReturnRoute(p.return_route),
    surfaceOps: Array.isArray(p.surface_ops) ? p.surface_ops.filter((o): o is string => typeof o === "string") : [],
    aglM: num(p.agl_m),
    aglSource: oneOf(p.agl_source, ["rangefinder", "home"] as const),
    statustext: parseStatusText(p.statustext),
    setupStep: str(p.setup_step),
    fcFailsafe: p.fc_failsafe === true,
    failsafeCause: str(p.failsafe_cause),
    setupAbort: str(p.setup_abort),
    lowBatteryAction: oneOf(p.low_battery_action, ["land", "rth"] as const),
    batteryReserve: parseBatteryReserve(p.battery_reserve),
    speedCap: parseSpeedCap(p.speed_cap),
    placeClass: oneOf(p.place_class, PLACE_CLASSES) ?? "open",
    rcReceiver: p.rc_receiver === "absent" ? "absent" : "present",
    routeCheck: parseRouteCheck(p.route_check),
    sensors: p.sensors ?? null,
    positionUnverified: parsePositionUnverified(p.position_unverified),
    poseUncertaintyM: num(p.pose_uncertainty_m),
    overheadAhead: parseOverheadAhead(p.overhead_ahead),
    sensing: parseSensing(p.sensing),
    goalKind: parseGoalKind(p.goal_kind),
    view: parseFollowView(p.view),
    movers: parseMovers(p.movers),
    profileLimits: parseProfileLimits(p.profile_limits),
    stick: parseStickCommand(p.stick),
    simClock: parseSimClock(p.clock),
    commandLimits: Array.isArray(obj(p.command)?.limits)
      ? (obj(p.command)!.limits as unknown[]).filter((l): l is string => typeof l === "string")
      : [],
  };
}
