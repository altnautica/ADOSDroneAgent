// The gate for single config-leaf writes that need a guard. A change the page
// marks guarded is held (nothing is sent) until the operator fires the guard;
// an unguarded change writes at once.
// Holding reuses the apply-form confirm gate, so the write that fires is the
// change that was held, and a cancel fires nothing.

import { openGate, type ConfirmGate } from "@/lib/settings/apply-form";

export interface LeafChange {
  key: string;
  value: string | number | boolean;
}

export type LeafStep =
  | { kind: "write"; change: LeafChange }
  | { kind: "hold"; gate: ConfirmGate<LeafChange> };

export function stepLeafChange(change: LeafChange, guarded: boolean): LeafStep {
  if (!guarded) return { kind: "write", change };
  return { kind: "hold", gate: openGate(change) };
}

/** How long a number row waits after the last step before it writes, so a
 *  held stepper sends one write rather than one per tick. */
export const NUMBER_COMMIT_IDLE_MS = 700;
