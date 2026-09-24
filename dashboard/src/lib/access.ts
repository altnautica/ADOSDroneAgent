// The dashboard's access check: whether this browser may read the agent's data
// plane right now. The access gate runs it on load and whenever a request is
// refused; it is the ONE place a stored session is judged dead and dropped.

import { ApiError, apiFetch, isAuthChallenge } from "./api";
import { clearSession } from "./session";

export type AccessVerdict = "ok" | "locked";

let inFlight: Promise<AccessVerdict> | null = null;

/**
 * Probe a gated route with whatever credential is stored.
 *
 * - 200 → `ok`: on-box, or a valid session / key.
 * - 401 (paired, credential missing or revoked) or 403 (unpaired, PIN needed)
 *   ON THIS PROBE → the stored session is not accepted, so it is dropped and
 *   the verdict is `locked`.
 * - Anything else (transient network, a 503) → `ok`: the panels surface their
 *   own errors rather than the whole dashboard blocking.
 *
 * Concurrent callers share one probe, so a burst of refused panel polls costs
 * one request.
 */
export function verifyAccess(): Promise<AccessVerdict> {
  if (inFlight) return inFlight;
  inFlight = (async (): Promise<AccessVerdict> => {
    try {
      await apiFetch("/api/status", { skipAuthSignal: true });
      return "ok";
    } catch (e) {
      if (e instanceof ApiError && isAuthChallenge(e.status)) {
        clearSession();
        return "locked";
      }
      return "ok";
    } finally {
      inFlight = null;
    }
  })();
  return inFlight;
}
