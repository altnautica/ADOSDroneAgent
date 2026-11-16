// Steady take-off on the surfaces: what a launch's lift-off is doing in the
// pilot's words (its status line, and the longer detail where a surface has
// room) or why it stopped on the ground, and the Balance check row's "I
// rebalanced it", which sends the pilot's `airframe_reset` so the airframe is
// learned anew. Pure.

import type { LaunchSequence, ReadinessItem } from "./cockpit-frames";

/** The pilot's Balance check readiness item id. */
export const AIRFRAME_BALANCE_ID = "airframe_balance";

/** The pilot action that clears the connected flight controller's learned
 *  airframe record. */
export const AIRFRAME_RESET_ACTION = "airframe_reset";

/** The reset's label ("I rebalanced it"). */
export const AIRFRAME_RESET_KEY = "readiness.airframe_balance.reset";

/** Whether the readiness item offers "I rebalanced it": the Balance check
 *  while it warns or blocks, on a vehicle known to be disarmed (`armed` null
 *  while unknown offers nothing). */
export function airframeResettable(item: Pick<ReadinessItem, "id" | "ok">, armed: boolean | null): boolean {
  return item.id === AIRFRAME_BALANCE_ID && !item.ok && armed === false;
}

/** A launch's lift-off: its status line and detail. */
export interface LiftoffLine {
  status: string;
  detail: string | null;
}

/** The lift-off under a launch: its phase in words while it runs, the stop
 *  once it stopped on the ground; null without a steady take-off, or for an
 *  error the lift-off did not raise. */
export function launchLiftoffLine(seq: LaunchSequence | null | undefined): LiftoffLine | null {
  const lo = seq?.liftoff;
  if (!seq || !lo?.text) return null;
  if (seq.error !== null && lo.phase !== "stopped") return null;
  return { status: lo.text, detail: lo.detail };
}

/** The lift-off as one line: the status, then the detail. */
export function liftoffLineText(l: LiftoffLine): string {
  return l.detail ? `${l.status}. ${l.detail}` : l.status;
}
