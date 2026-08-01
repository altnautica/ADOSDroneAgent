// Polls the agent status at 2–5 Hz and hands the latest snapshot to the status
// strip + screens. The source is profile-aware: a ground station reads its
// single composite (`/api/v1/ground-station/status`); a drone has no such
// endpoint, so it composes an equivalent from its own status/radio/telemetry
// reads (`getDroneStatus`). On a failed poll it keeps the last snapshot and
// flips `stale` so surfaces dim honestly rather than blank or fabricate. Uses a
// self-scheduling timeout so a slow poll never overlaps the next.

import { useEffect, useRef, useState } from "react";

import { pollIntervalMs, renderProfile } from "@/lib/render-profile";

import { useProfile } from "@/hooks/use-profile";
import { ApiError, getDroneStatus, getGsStatus } from "@/lib/api";
import type { GsStatus } from "@/lib/types";
import { pollIntervalFor, useReachStore } from "@/stores/reach-store";

export interface TelemetryState {
  status: GsStatus | null;
  error: string | null;
  /** True when the most recent poll failed (the snapshot may be old). */
  stale: boolean;
}

/** Poll agent status every `intervalMs` (default 400 ms ≈ 2.5 Hz). The source
 *  follows the agent profile: a drone composes its own status, everything else
 *  reads the ground-station composite. */
export function useTelemetry(intervalMs = pollIntervalMs(400, renderProfile())): TelemetryState {
  const profile = useProfile();
  const [state, setState] = useState<TelemetryState>({
    status: null,
    error: null,
    stale: false,
  });
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    let cancelled = false;
    const controller = new AbortController();
    const fetchStatus =
      profile === "drone" ? getDroneStatus : getGsStatus;

    const tick = async () => {
      try {
        const status = await fetchStatus(controller.signal);
        if (cancelled) return;
        useReachStore.getState().report(null, true);
        setState({ status, error: null, stale: false });
      } catch (err) {
        if (cancelled || controller.signal.aborted) return;
        // Refused is not absent: report it so the shell names the cause.
        useReachStore
          .getState()
          .report(err instanceof ApiError ? err.status : null, false);
        setState((prev) => ({
          status: prev.status,
          error: err instanceof Error ? err.message : String(err),
          stale: true,
        }));
      } finally {
        if (!cancelled) {
          // Back off while the agent is refusing — see `pollIntervalFor`.
          const wait = pollIntervalFor(
            intervalMs,
            useReachStore.getState().refusal,
          );
          timer.current = setTimeout(tick, wait);
        }
      }
    };

    void tick();

    return () => {
      cancelled = true;
      controller.abort();
      if (timer.current) clearTimeout(timer.current);
    };
  }, [intervalMs, profile]);

  return state;
}
