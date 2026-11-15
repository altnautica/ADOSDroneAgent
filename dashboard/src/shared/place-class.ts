// The place-class picker's explain line on the agent's own pages (cockpit and
// dashboard): the obstacle margin the pilot would fly with in each class. In
// the confined class the pilot's route check reports the margin it uses; else
// the margin comes from the agent config (`pilot.obstacle_margin_m`,
// `pilot.prop_tip_radius_m`), raised to the class floor.

import type { PilotState, PlaceClass } from "./hud/cockpit-frames";
import { effectiveMarginM, placeClassExplain } from "./hud/route-check";

export interface PlaceMargins {
  /** `pilot.obstacle_margin_m`; null when the config does not carry it. */
  obstacleMarginM: number | null;
  /** `pilot.prop_tip_radius_m`; null while unset. */
  propTipRadiusM: number | null;
}

function finite(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

/** The margin inputs from the agent config tree (`GET /api/config`). */
export function placeMargins(config: unknown): PlaceMargins {
  const pilot = typeof config === "object" && config !== null ? (config as Record<string, unknown>).pilot : null;
  const p = typeof pilot === "object" && pilot !== null ? (pilot as Record<string, unknown>) : {};
  return { obstacleMarginM: finite(p.obstacle_margin_m), propTipRadiusM: finite(p.prop_tip_radius_m) };
}

/** The explain line for `cls`: the route check's margin while the pilot is
 *  already confined and reports one, else the effective margin from config. */
export function placeExplainLine(cls: PlaceClass, pilot: Pick<PilotState, "placeClass" | "routeCheck"> | null, margins: PlaceMargins): string {
  const live = cls === "confined" && pilot?.placeClass === "confined" ? (pilot.routeCheck?.marginM ?? null) : null;
  return placeClassExplain(cls, live ?? effectiveMarginM(cls, margins.obstacleMarginM, margins.propTipRadiusM));
}
