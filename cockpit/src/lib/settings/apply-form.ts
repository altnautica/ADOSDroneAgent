// Shared logic for the settings pages that send a slice to the batch apply
// endpoint (`POST /api/v1/setup/apply`): the confirm gate that holds a request
// until the operator slides, and the read of one section's verdict.
//
// The gate snapshots the payload when the operator asks to apply, so the
// request that fires is exactly the one the confirm text described, and a
// cancel or a confirm with nothing pending never fires anything.

import type { ApplyResponse } from "@/shared/api/apply-actions";

export type ConfirmGate<P> = { open: false } | { open: true; payload: P };

export const CLOSED_GATE: ConfirmGate<never> = { open: false };

/** Hold `payload` behind the confirm. Nothing is sent yet. */
export function openGate<P>(payload: P): ConfirmGate<P> {
  return { open: true, payload };
}

/** The payload to send when the gate is settled: only an open gate that was
 *  confirmed yields one. */
export function settleGate<P>(gate: ConfirmGate<P>, confirmed: boolean): P | null {
  return gate.open && confirmed ? gate.payload : null;
}

export interface SectionVerdict {
  ok: boolean;
  /** The agent's own message for the section, when it sent a non-empty one. */
  message: string | null;
  restartRequired: boolean;
}

/** One section's verdict from an apply reply. HTTP 200 is not success: the
 *  reply must say `overall` and the section must say `ok`. */
export function sectionVerdict(res: ApplyResponse | null | undefined, section: string): SectionVerdict {
  const s = res?.sections?.[section];
  const ok = res?.overall === true && s?.ok === true;
  return {
    ok,
    message: s?.message ? s.message : null,
    restartRequired: ok && s?.data?.restart_required === true,
  };
}
