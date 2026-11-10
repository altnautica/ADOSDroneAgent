// The detection lease an on-screen target overlay holds on its camera. The
// vision engine runs detection on a camera only while some consumer holds a
// lease on it, so the overlay renews one every RENEW_MS for HOLD_S seconds
// (three renewals of slack) and releases it (`hold_s: 0`) when it closes or
// switches camera. Each answer says whether the engine is detecting there,
// which the overlay shows instead of an empty frame that looks broken.

import type { DetectionDemandResponse } from "@/lib/api";
import { specText } from "@/shared/hud/spec-text";

/** Lease length per request, seconds. */
export const LEASE_HOLD_S = 6;
/** Renewal period, ms. */
export const LEASE_RENEW_MS = 2000;

/** What the overlay knows about detection on its camera.
 *  - `starting`: the first request has not answered yet;
 *  - `detecting`: the engine runs detection on this camera;
 *  - `idle`: the engine answered and is not detecting here;
 *  - `unavailable`: the request failed (no vision engine, camera not captured). */
export type DetectorState = "starting" | "detecting" | "idle" | "unavailable";

export type DemandRequest = (cameraId: string, holdS: number) => Promise<DetectionDemandResponse>;

/**
 * Hold a lease on `cameraId` until the returned stop function runs: one
 * request now, one every `LEASE_RENEW_MS`, and a release on stop. `onState`
 * hears each answer; nothing is reported after stop.
 */
export function startDetectionLease(
  cameraId: string,
  request: DemandRequest,
  onState: (state: DetectorState) => void,
): () => void {
  let stopped = false;
  const renew = () => {
    request(cameraId, LEASE_HOLD_S).then(
      (r) => {
        if (!stopped) onState(r.detecting ? "detecting" : "idle");
      },
      () => {
        if (!stopped) onState("unavailable");
      },
    );
  };
  onState("starting");
  renew();
  const timer = setInterval(renew, LEASE_RENEW_MS);
  return () => {
    if (stopped) return;
    stopped = true;
    clearInterval(timer);
    // Best effort: an unanswered release still lapses after LEASE_HOLD_S.
    request(cameraId, 0).catch(() => undefined);
  };
}

/** The overlay's notice for a detector state (`detector.*`), or null when
 *  boxes speak for themselves (detecting) or nothing is known yet. */
export function detectorNotice(state: DetectorState | null): string | null {
  return state === "idle" || state === "unavailable" ? specText(`detector.${state}`) : null;
}
