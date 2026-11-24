/**
 * @module hud/cockpit-ar
 * @description The cockpit v2 nav-stream kind `ar`: the projected route and
 * motion items per camera, with stable item keys, plus the avoidance states
 * the pilot frame and the follow path share. Parsed apart from
 * `cockpit-frames.ts`, which re-exports everything here.
 *
 * Dependency-free on purpose apart from its sibling `cockpit-mission.ts`:
 * Mission Control copies this file byte for byte.
 *
 * @license GPL-3.0-only
 */

import { parseMission, type MissionState } from "./cockpit-mission";

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
function bool(v: unknown): boolean | null {
  return typeof v === "boolean" ? v : null;
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
function pts2(v: unknown): [number, number][] {
  if (!Array.isArray(v)) return [];
  const out: [number, number][] = [];
  for (const p of v) {
    const q = pt2(p);
    if (q) out.push(q);
  }
  return out;
}

export type AvoidState = "clear" | "detour" | "blocked" | "off" | "stale" | "unavailable";
export const AVOID_STATES: readonly AvoidState[] = ["clear", "detour", "blocked", "off", "stale", "unavailable"];

export type ArItem =
  | { t: "wp"; k: string; seq: number; role: "next" | "after" | "other"; x: number; y: number; gx: number | null; gy: number | null; distM: number | null; etaS: number | null; size: number }
  | { t: "leg"; k: string; role: "active" | "next" | "future"; pts: [number, number][] }
  | { t: "home"; k: string; x: number; y: number; gx: number | null; gy: number | null; distM: number | null }
  | { t: "fpv"; k: string; x: number; y: number }
  | { t: "fpath"; k: string; state: AvoidState | null; pts: [number, number][] }
  | { t: "fgoal"; k: string; x: number; y: number }
  /** The follow search ellipse as a ground ring (drawn closed). */
  | { t: "fsearch"; k: string; pts: [number, number][] }
  /** The pilot's vantage point on the ground while the follow searches. */
  | { t: "fvantage"; k: string; x: number; y: number }
  | { t: "orbit"; k: string; dir: "cw" | "ccw"; cx: number; cy: number; pts: [number, number][] }
  | { t: "edge"; k: string; of: "wp" | "home"; seq: number | null; ang: number; distM: number | null };

export interface ArCamera {
  frameW: number;
  frameH: number;
  reason: string | null;
  items: ArItem[];
}

export interface ArFrame {
  tsMs: number;
  approx: boolean;
  reason: string | null;
  mission: MissionState | null;
  cameras: Record<string, ArCamera>;
}

function parseArItem(raw: unknown): ArItem | null {
  const o = obj(raw);
  if (!o) return null;
  const k = str(o.k);
  switch (o.t) {
    case "wp": {
      const seq = num(o.seq);
      const role = oneOf(o.role, ["next", "after", "other"] as const);
      const x = num(o.x);
      const y = num(o.y);
      if (seq === null || !role || x === null || y === null) return null;
      return { t: "wp", k: k ?? `wp:${seq}`, seq, role, x, y, gx: num(o.gx), gy: num(o.gy), distM: num(o.dist_m), etaS: num(o.eta_s), size: num(o.size) ?? 0 };
    }
    case "leg": {
      const role = oneOf(o.role, ["active", "next", "future"] as const);
      const pts = pts2(o.pts);
      if (!role || pts.length < 2) return null;
      return { t: "leg", k: k ?? `leg:${role}`, role, pts };
    }
    case "home": {
      const x = num(o.x);
      const y = num(o.y);
      if (x === null || y === null) return null;
      return { t: "home", k: k ?? "home", x, y, gx: num(o.gx), gy: num(o.gy), distM: num(o.dist_m) };
    }
    case "fpv":
    case "fgoal":
    case "fvantage": {
      const x = num(o.x);
      const y = num(o.y);
      if (x === null || y === null) return null;
      return { t: o.t, k: k ?? o.t, x, y };
    }
    case "fpath": {
      const pts = pts2(o.pts);
      if (pts.length < 2) return null;
      return { t: "fpath", k: k ?? "fpath", state: oneOf(o.state, AVOID_STATES), pts };
    }
    case "fsearch": {
      const pts = pts2(o.pts);
      if (pts.length < 3) return null;
      return { t: "fsearch", k: k ?? "fsearch", pts };
    }
    case "orbit": {
      const cx = num(o.cx);
      const cy = num(o.cy);
      const pts = pts2(o.pts);
      if (cx === null || cy === null || pts.length < 2) return null;
      return { t: "orbit", k: k ?? "orbit", dir: o.dir === "ccw" ? "ccw" : "cw", cx, cy, pts };
    }
    case "edge": {
      const of = oneOf(o.of, ["wp", "home"] as const);
      const ang = num(o.ang);
      if (!of || ang === null) return null;
      const seq = num(o.seq);
      return { t: "edge", k: k ?? `edge:${of === "home" ? "home" : `wp:${seq ?? "?"}`}`, of, seq, ang, distM: num(o.dist_m) };
    }
    default:
      return null;
  }
}

export function parseArFrame(raw: unknown): ArFrame | null {
  const o = obj(raw);
  const cams = o ? obj(o.cameras) : null;
  if (!o || !cams) return null;
  const cameras: Record<string, ArCamera> = {};
  for (const id of Object.keys(cams)) {
    const c = obj(cams[id]);
    if (!c) continue;
    const frameW = num(c.frame_w);
    const frameH = num(c.frame_h);
    if (frameW === null || frameH === null || frameW <= 0 || frameH <= 0) continue;
    const items: ArItem[] = [];
    if (Array.isArray(c.items)) {
      for (const it of c.items) {
        const item = parseArItem(it);
        if (item) items.push(item);
      }
    }
    cameras[id] = { frameW, frameH, reason: str(c.reason), items };
  }
  return {
    tsMs: num(o.ts_ms) ?? 0,
    approx: bool(o.approx) ?? false,
    reason: str(o.reason),
    mission: parseMission(o.mission),
    cameras,
  };
}
