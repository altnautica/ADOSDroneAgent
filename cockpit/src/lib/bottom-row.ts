// The bottom row's values (cockpit spec `layout.json` zone `bottom_row`: D, H,
// H.S, V.S, AGL), worked out the same way on every cockpit. D is the distance
// from home: the agent's own figure when it sends one, else the great-circle
// distance from the home point to the GNSS position, else, with no GNSS, the
// horizontal distance of the flight controller's local position from its EKF
// origin. AGL comes from the pilot state while it is fresh; once the pilot
// frame is older than `stale_after_ms` the cell dims and reads a dash rather
// than showing a frozen height.

import { LOCAL_POSITION_FRESH_MS } from "@/lib/local-minimap";
import type { VehicleState } from "@/lib/types";
import { haversineM } from "@/shared/format";
import { bottomRowAgl, type BottomRowAgl } from "@/shared/hud/chip-inputs";
import type { PilotState } from "@/shared/hud/cockpit-frames";
import { CHIP_STALE_AFTER_MS } from "@/shared/hud/flight-state";

const finite = (v: number | null | undefined): v is number => typeof v === "number" && Number.isFinite(v);

/** Metres from home, or null when no source backs one. */
export function distanceFromHomeM(t: VehicleState | null, home: { lat: number; lon: number } | null): number | null {
  if (!t) return null;
  if (finite(t.home_distance)) return t.home_distance;
  const lat = t.position?.lat;
  const lon = t.position?.lon;
  if (home && finite(lat) && finite(lon) && (lat !== 0 || lon !== 0)) return haversineM(home.lat, home.lon, lat, lon);
  const lp = t.local_position;
  if (lp && lp.age_ms <= LOCAL_POSITION_FRESH_MS && finite(lp.x) && finite(lp.y)) return Math.hypot(lp.x, lp.y);
  return null;
}

export interface RowAgl extends BottomRowAgl {
  /** The pilot frame behind the value has gone stale: dim the cell. */
  stale: boolean;
}

/** The AGL cell: the pilot's height while its frame is fresh, the follow
 *  kind's when no pilot runs, and a dimmed dash once the pilot stalls. */
export function rowAgl(
  pilot: { data: Pick<PilotState, "aglM" | "aglSource">; at: number } | null,
  follow: { aglM: number | null; aglSource: "rng" | "baro" | null } | null,
  nowMs: number,
): RowAgl {
  if (pilot && nowMs - pilot.at > CHIP_STALE_AFTER_MS) return { m: null, mark: null, stale: true };
  return { ...bottomRowAgl(pilot?.data ?? null, follow), stale: false };
}
