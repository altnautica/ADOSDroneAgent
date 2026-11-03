// Cloud pairing writes for the Pair screen, plus the guard each Pair action
// carries. The accept route is HTTP-200-on-failure: the outcome is in `ok`,
// so a `{ok:false}` body is a refusal, never "paired".

import { apiFetch } from "@/shared/api-fetch";

/** `POST /api/pairing/accept` body. */
export interface AcceptCodeResponse {
  ok?: boolean;
  error?: string | null;
  message?: string | null;
}

/** The refusal text in an accept response, or null when the agent paired. */
export function acceptRefusal(res: AcceptCodeResponse | null | undefined, fallback: string): string | null {
  if (res && res.ok === true) return null;
  return res?.message || res?.error || fallback;
}

/** A pairing code as typed: trimmed, inner whitespace removed. Null when empty. */
export function normalizePairingCode(raw: string): string | null {
  const code = raw.replace(/\s+/g, "");
  return code ? code : null;
}

/** Accept a Mission Control pairing code. Throws with the agent's reason when
 *  the agent refuses it. */
export async function acceptPairingCode(code: string, fallback: string): Promise<void> {
  const res = await apiFetch<AcceptCodeResponse>("/api/pairing/accept", { method: "POST", body: { code } });
  const refusal = acceptRefusal(res, fallback);
  if (refusal) throw new Error(refusal);
}

/** Release this node from its Mission Control owner. */
export async function unpairCloud(): Promise<void> {
  await apiFetch("/api/pairing/unpair", { method: "POST", body: {} });
}

/** The confirm gate on each Pair action (ledger P3). Unpairing cuts the
 *  owner's access or the radio key, so both are guarded; the rest are not
 *  destructive. */
export const PAIR_ACTION_GUARD = {
  cloudAccept: "none",
  cloudUnpair: "guarded",
  rfBind: "none",
  rfUnpair: "guarded",
  phoneApprove: "none",
  phoneReject: "none",
} as const satisfies Record<string, "none" | "guarded" | "slide">;
