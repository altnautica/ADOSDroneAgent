// The mission on the drone, for the cockpit map: the router's mission mirror
// (`GET /api/v1/mission`, every MISSION_ITEM_INT field) reduced to the
// positioned items in flight order. Fetched while the FC reports a mission and
// again when its id or item count changes.

import { useEffect, useState } from "react";

import { apiFetch } from "@/shared/api-fetch";
import type { LatLon } from "@/lib/minimap-geometry";
import { useFlightStore } from "@/stores/flight-store";

export interface MissionPoint extends LatLon {
  seq: number;
}

/** MAV_CMD values whose x/y are a position the vehicle flies to. */
const POSITION_CMDS = new Set([16, 17, 18, 19, 21, 22, 31, 82, 84, 85]);
/** MAV_FRAME values whose x/y are latitude/longitude. */
const GLOBAL_FRAMES = new Set([0, 3, 5, 6, 10, 11]);

/** The positioned items of a `/api/v1/mission` body in seq order. The home
 *  slot (seq 0 when `home_slot`) is not a waypoint, and a 0/0 position is an
 *  unset item. */
export function parseMissionRoute(body: unknown): MissionPoint[] {
  if (!body || typeof body !== "object") return [];
  const b = body as { present?: unknown; home_slot?: unknown; items?: unknown };
  if (b.present !== true || !Array.isArray(b.items)) return [];
  const out: MissionPoint[] = [];
  for (const raw of b.items) {
    if (!raw || typeof raw !== "object") continue;
    const it = raw as Record<string, unknown>;
    const { seq, command, frame, x, y } = it;
    if (typeof seq !== "number" || typeof x !== "number" || typeof y !== "number") continue;
    if (b.home_slot === true && seq === 0) continue;
    if (!POSITION_CMDS.has(command as number) || !GLOBAL_FRAMES.has(frame as number)) continue;
    if (x === 0 && y === 0) continue;
    out.push({ seq, lat: x / 1e7, lon: y / 1e7 });
  }
  return out.sort((a, b) => a.seq - b.seq);
}

/** The mission route while the FC reports one, else an empty list. */
export function useMissionRoute(): MissionPoint[] {
  const key = useFlightStore((s) => {
    const m = s.telemetry?.mission;
    const state = typeof m?.state === "string" ? m.state : "none";
    return state === "none" ? null : `${String(m?.id ?? "")}:${String(m?.total ?? "")}`;
  });
  const [route, setRoute] = useState<{ key: string; points: MissionPoint[] } | null>(null);

  useEffect(() => {
    if (key === null) return;
    const ctl = new AbortController();
    apiFetch<unknown>("/api/v1/mission", { signal: ctl.signal })
      .then((body) => setRoute({ key, points: parseMissionRoute(body) }))
      .catch(() => {});
    return () => ctl.abort();
  }, [key]);

  return key !== null && route?.key === key ? route.points : [];
}
