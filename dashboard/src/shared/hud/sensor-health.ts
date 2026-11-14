// Navigation sensor health as every surface shows it (cockpit spec
// `sensor-health.json`): the flight controller's optical flow and downward
// range, the scanning lidar, and the cameras. The pilot kind carries `sensors`
// {flow, range, lidar}; GET /api/v1/sensors carries `health` {flow, range,
// lidar, cameras}. The agent classifies; this module narrows the wire JSON
// (an unknown state word reads as absent, a camera's as lost) and turns each
// part into its spec word and colour. A null flow or range part (the agent
// has no fresh word from the flight controller) reads unknown: "—" in the
// stale colour, never "No flow". Values format as the spec's `format` says;
// a missing value reads "—". Behaviour cases:
// fixtures/sensor-health-cases.json.

import specJson from "../cockpit-spec/sensor-health.json";
import tokensJson from "../cockpit-spec/tokens.json";

export type FlowState = "ok" | "low" | "lost" | "absent" | "unknown";
export type RangeState = "ok" | "out_of_range" | "lost" | "absent" | "unknown";
export type LidarState = "ok" | "degraded" | "lost" | "absent";
export type CameraState = "live" | "stale" | "lost";
export type LidarDriverHealth = "good" | "warning" | "error";

export interface FlowHealth {
  state: FlowState;
  quality: number | null;
  rateHz: number | null;
  ageMs: number | null;
}

export interface RangeHealth {
  state: RangeState;
  heightM: number | null;
  ageMs: number | null;
}

export interface LidarHealth {
  state: LidarState;
  rateHz: number | null;
  health: LidarDriverHealth | null;
  ageMs: number | null;
}

/** The pilot kind's `sensors`. */
export interface SensorHealth {
  flow: FlowHealth;
  range: RangeHealth;
  lidar: LidarHealth;
}

export interface CameraHealth {
  id: string;
  /** The stream leg the camera feeds (`main` for the primary). */
  leg: string | null;
  role: string | null;
  state: CameraState;
  fps: number | null;
  ageMs: number | null;
}

/** GET /api/v1/sensors `health` (the mounts are not shown, so not read). */
export interface SensorsHealth extends SensorHealth {
  cameras: CameraHealth[];
}

/** One part as a surface shows it: the spec text and its colour (a
 *  tokens.json `state.*` name and that token's hex). */
export interface SensorWord {
  state: string;
  text: string;
  color: string;
  hex: string;
}

type Obj = Record<string, unknown>;

function obj(v: unknown): Obj | null {
  return typeof v === "object" && v !== null && !Array.isArray(v) ? (v as Obj) : null;
}
function num(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}
function str(v: unknown): string | null {
  return typeof v === "string" && v !== "" ? v : null;
}
function oneOf<T extends string>(v: unknown, values: readonly T[]): T | null {
  return typeof v === "string" && (values as readonly string[]).includes(v) ? (v as T) : null;
}

const FLOW_STATES = ["ok", "low", "lost", "absent"] as const;
const RANGE_STATES = ["ok", "out_of_range", "lost", "absent"] as const;
const LIDAR_STATES = ["ok", "degraded", "lost", "absent"] as const;
const CAMERA_STATES = ["live", "stale", "lost"] as const;

export function parseFlowHealth(raw: unknown): FlowHealth {
  const o = obj(raw) ?? {};
  if (raw === null || raw === undefined) return { state: "unknown", quality: null, rateHz: null, ageMs: null };
  return { state: oneOf(o.state, FLOW_STATES) ?? "absent", quality: num(o.quality), rateHz: num(o.rate_hz), ageMs: num(o.age_ms) };
}

export function parseRangeHealth(raw: unknown): RangeHealth {
  const o = obj(raw) ?? {};
  if (raw === null || raw === undefined) return { state: "unknown", heightM: null, ageMs: null };
  return { state: oneOf(o.state, RANGE_STATES) ?? "absent", heightM: num(o.height_m), ageMs: num(o.age_ms) };
}

export function parseLidarHealth(raw: unknown): LidarHealth {
  const o = obj(raw) ?? {};
  return {
    state: oneOf(o.state, LIDAR_STATES) ?? "absent",
    rateHz: num(o.rate_hz),
    health: oneOf(o.health, ["good", "warning", "error"] as const),
    ageMs: num(o.age_ms),
  };
}

/** A camera entry, or null without an id. */
export function parseCameraHealth(raw: unknown): CameraHealth | null {
  const o = obj(raw);
  const id = o ? str(o.id) : null;
  if (!o || id === null) return null;
  return { id, leg: str(o.leg), role: str(o.role), state: oneOf(o.state, CAMERA_STATES) ?? "lost", fps: num(o.fps), ageMs: num(o.age_ms) };
}

/** The pilot kind's `sensors`; null when the agent sends none (no FC link). */
export function parseSensorHealth(raw: unknown): SensorHealth | null {
  const o = obj(raw);
  if (!o) return null;
  return { flow: parseFlowHealth(o.flow), range: parseRangeHealth(o.range), lidar: parseLidarHealth(o.lidar) };
}

/** GET /api/v1/sensors `health`; null when the body carries none. */
export function parseSensorsHealth(raw: unknown): SensorsHealth | null {
  const o = obj(raw);
  const base = parseSensorHealth(o);
  if (!o || !base) return null;
  const cameras = Array.isArray(o.cameras) ? o.cameras.map(parseCameraHealth).filter((c): c is CameraHealth => c !== null) : [];
  return { ...base, cameras };
}

// ── words ─────────────────────────────────────────────────────────────────

const NO_DATA = "—";
const STATE_HEX: Readonly<Record<string, string>> = tokensJson.state as Record<string, string>;
type StateTable = Readonly<Record<string, { text: string; color: string }>>;
const STATES = specJson.states as unknown as Readonly<Record<"flow" | "range" | "lidar" | "camera", StateTable>>;

const fmtInt = (v: number | null): string => (v === null ? NO_DATA : String(Math.round(v)));
const fmt1 = (v: number | null): string => (v === null ? NO_DATA : v.toFixed(1));

function word(table: StateTable, state: string, args: Readonly<Record<string, string>>): SensorWord {
  const entry = table[state];
  if (state === "unknown") return { state, text: NO_DATA, color: "stale", hex: STATE_HEX.stale ?? "#909090" };
  const text = entry.text.replace(/\{([a-z_]+)\}/g, (_, k: string) => args[k] ?? "");
  return { state, text, color: entry.color, hex: STATE_HEX[entry.color] ?? "#909090" };
}

export function flowWord(f: FlowHealth): SensorWord {
  return word(STATES.flow, f.state, { quality: fmtInt(f.quality) });
}

export function rangeWord(r: RangeHealth): SensorWord {
  return word(STATES.range, r.state, { height_m: fmt1(r.heightM) });
}

export function lidarWord(l: LidarHealth): SensorWord {
  return word(STATES.lidar, l.state, { rate_hz: fmt1(l.rateHz) });
}

/** A camera's word, named `name` (a surface's label for the leg) or its id. */
export function cameraWord(c: CameraHealth, name: string = c.id): SensorWord {
  return word(STATES.camera, c.state, { id: name, fps: fmtInt(c.fps) });
}

/** The bottom row's sensor cells beside AGL: the flow word, then the range
 *  word, each only when the part is not absent. */
export function sensorRowWords(s: SensorHealth | null): SensorWord[] {
  if (!s) return [];
  const out: SensorWord[] = [];
  if (s.flow.state !== "absent" && s.flow.state !== "unknown") out.push(flowWord(s.flow));
  if (s.range.state !== "absent" && s.range.state !== "unknown") out.push(rangeWord(s.range));
  return out;
}

/** A stream tab's or picture-in-picture label's camera word: stale or lost
 *  only (a live camera adds nothing), named by the tab's label. */
export function cameraTabWord(c: CameraHealth | null, label: string): SensorWord | null {
  return c && c.state !== "live" ? cameraWord(c, label) : null;
}

/** The health entry behind a stream leg: by the leg it feeds, else the
 *  camera id, else a role only one camera has. */
export function cameraForLeg(cameras: readonly CameraHealth[], leg: { id: string; role?: string | null }): CameraHealth | null {
  const byLeg = cameras.find((c) => c.leg === leg.id);
  if (byLeg) return byLeg;
  const byId = cameras.find((c) => c.id === leg.id);
  if (byId) return byId;
  if (!leg.role) return null;
  const byRole = cameras.filter((c) => c.role === leg.role);
  return byRole.length === 1 ? byRole[0] : null;
}
