// Why the panel has no data, when the reason is that the agent refused us
// rather than that a value is genuinely absent.
//
// Every poll in the cockpit degrades the same honest way on failure: keep the
// last snapshot and flip `stale`, so a surface dims rather than blanks or
// fabricates. That is right for a link that dropped and wrong for a node that
// refused the request, because the two look identical on screen — em dashes
// everywhere — while only one of them has anything the operator can do about
// it.
//
// The case that made this necessary: a node that has not been paired refuses
// every `/api/*` call from an ordinary network peer. The shell still renders,
// because the route that answers "who are you" is deliberately public, so the
// operator sees a full cockpit with every field dashed and no statement
// anywhere that the node is simply not paired yet — while the code that would
// pair it is sitting in the response the shell already has.
//
// So the refusal is reported here, once, and the shell says it plainly.

import { create } from "zustand";

/** Why the agent is refusing, as far as a client can tell from a status code. */
export type ReachRefusal =
  /** Nothing is being refused. */
  | "none"
  /** The node has not been paired, so it answers only its own identity. */
  | "unpaired"
  /** The node is paired and this browser holds no credential for it. */
  | "unauthorized";

interface ReachState {
  refusal: ReachRefusal;
  /** The node's pairing code, read from the public identity route so the
   *  operator can act on the refusal instead of just reading about it. */
  pairingCode: string | null;
  /** Report the outcome of any agent call. Success clears the refusal, so a
   *  node that gets paired while the panel is open recovers on the next poll
   *  with no reload. */
  report: (status: number | null, ok: boolean) => void;
  setPairingCode: (code: string | null) => void;
}

/** Whether a status code means "the agent refused this", as opposed to a route
 *  that does not exist on this profile (404) or a server-side fault (5xx). */
export function isRefusal(status: number | null): boolean {
  return status === 401 || status === 403;
}

/** How long a poll should wait after a refusal, in ms.
 *
 *  A refusal will not resolve on its own — it takes an operator pairing the
 *  node or signing in — so retrying at the live-telemetry cadence spends real
 *  effort on an answer that cannot change yet. Measured on a node refusing an
 *  off-box browser: 512 rejected requests inside two minutes, EACH of which the
 *  agent records as a warning, so the panel was also filling the log of the box
 *  it could not read.
 *
 *  Long enough to stop being a load, short enough that pairing feels immediate:
 *  the operator watches the panel while they pair, and five seconds is inside
 *  the time it takes them to type the code.
 */
export const REFUSED_POLL_INTERVAL_MS = 5000;

/** The interval a poll should use next, given its normal cadence and whether
 *  the agent is currently refusing. Never speeds a poll UP — a screen that
 *  deliberately polls slowly keeps its own pace. */
export function pollIntervalFor(normalMs: number, refusal: ReachRefusal): number {
  return refusal === "none" ? normalMs : Math.max(normalMs, REFUSED_POLL_INTERVAL_MS);
}

export const useReachStore = create<ReachState>((set) => ({
  refusal: "none",
  pairingCode: null,

  report: (status, ok) =>
    set((s) => {
      if (ok) {
        return s.refusal === "none" ? s : { ...s, refusal: "none" };
      }
      if (!isRefusal(status)) {
        // A 404 is a route this profile does not serve and a 5xx is the
        // agent's own fault; neither is a credential problem and neither
        // should raise a pairing prompt.
        return s;
      }
      // 403 is what an unpaired node answers a peer it will not talk to; 401
      // is what a paired node answers a caller with no key. The distinction
      // matters because the operator's next step is different.
      const refusal: ReachRefusal = status === 401 ? "unauthorized" : "unpaired";
      return s.refusal === refusal ? s : { ...s, refusal };
    }),

  setPairingCode: (code) =>
    set((s) => (s.pairingCode === code ? s : { ...s, pairingCode: code })),
}));
