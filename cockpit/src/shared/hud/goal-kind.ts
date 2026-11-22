/**
 * @module hud/goal-kind
 * @description The goal chip, the same words on every surface: which goal
 * the flying skill sent the pilot (`goal_kind`) and, for a follow intent, the
 * viewpoint the pilot picked (`view`): "Follow intent · view 15° 8 m",
 * "Ring", "Orbit". Nothing while no skill goal is reported (an older agent).
 *
 * @license GPL-3.0-only
 */

import type { FollowView, GoalKind } from "./pilot-fields";
import { specText } from "./spec-text";

/** `Follow intent`, `Ring`, `Orbit`, `Position` or `Vantage`. */
export function goalKindLabel(kind: GoalKind): string {
  return specText(`pilot.goal_kind.${kind}`);
}

/** `Follow intent · view 15° 8 m` while the pilot reports the viewpoint it
 *  picked, else the kind's label; null with no goal kind reported. */
export function goalKindChipText(kind: GoalKind | null, view: FollowView | null): string | null {
  if (!kind) return null;
  const label = goalKindLabel(kind);
  if (kind !== "follow_intent" || !view) return label;
  const bearing = Math.round(view.bearingDeg) % 360;
  return specText("pilot.goal_kind.view", { kind: label, bearing, r: Math.round(view.radiusM) });
}
