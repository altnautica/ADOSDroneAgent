// Cloud pairing writes for the Pair screen, plus the guard each Pair action
// carries. The accept route is HTTP-200-on-failure: the outcome is in `ok`,
// so a `{ok:false}` body is a refusal, never "paired".

import type { BindSession } from "@/lib/bind-phase";
import { isArmedRefusal, radioWriteBlock, type ArmedReading } from "@/lib/radio-gate";
import { apiFetch } from "@/shared/api-fetch";
import { withArmedRetry } from "@/shared/api/armed-retry";

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

const UNPAIR_PATHS = { cloud: "/api/pairing/unpair", rf: "/api/wfb/pair/unpair" } as const;

/** Release this node from its Mission Control owner (`cloud`) or its radio
 *  peer (`rf`). The agent refuses either while the vehicle may be armed; that
 *  refusal asks `confirm`, and a yes repeats it with `?force=1`. Resolves
 *  `done`, or, when the operator declines, why it was held back: the arm state
 *  `readArmed` reports, read as unknown when the snapshot says disarmed (the
 *  agent's refusal is newer). Any other failure throws. */
export async function unpair(
  target: keyof typeof UNPAIR_PATHS,
  confirm: () => Promise<boolean>,
  readArmed: () => Promise<ArmedReading | null>,
): Promise<"done" | "armed" | "armedUnknown"> {
  const path = UNPAIR_PATHS[target];
  try {
    await withArmedRetry((force) => apiFetch(force ? `${path}?force=1` : path, { method: "POST", body: {} }), confirm);
    return "done";
  } catch (err) {
    if (!isArmedRefusal(err)) throw err;
    return radioWriteBlock(await readArmed().catch(() => null)) ?? "armedUnknown";
  }
}

/** The bind steps that still wait on the peer, which Cancel can stop. */
const CANCELLABLE_BIND = new Set(["opening_tunnel", "waiting_peer"]);

/** Whether the agent's bind session (`GET /api/wfb/pair/local-bind`) is
 *  waiting, so Cancel shows whichever tab started it. */
export function bindCancellable(b: BindSession | null | undefined): boolean {
  const state = b?.state ?? b?.phase;
  return typeof state === "string" && CANCELLABLE_BIND.has(state);
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
