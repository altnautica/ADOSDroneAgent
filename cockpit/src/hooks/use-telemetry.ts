// Polls the agent status at 2–5 Hz and hands the latest snapshot to the status
// strip + screens. The source is profile-aware: a ground station reads its
// single composite (`/api/v1/ground-station/status`); a drone has no such
// endpoint, so it composes an equivalent from its own status/radio/telemetry
// reads (`getDroneStatus`). On a failed poll it keeps the last snapshot and
// flips `stale` so surfaces dim honestly rather than blank or fabricate. Uses a
// self-scheduling timeout so a slow poll never overlaps the next.

import { useEffect, useRef, useState } from "react";

import { useProfile } from "@/hooks/use-profile";
import { getDroneStatus, getGsStatus } from "@/lib/api";
import type { GsStatus } from "@/lib/types";

export interface TelemetryState {
  status: GsStatus | null;
  error: string | null;
  /** True when the most recent poll failed (the snapshot may be old). */
  stale: boolean;
}

/** Poll agent status every `intervalMs` (default 400 ms ≈ 2.5 Hz). The source
 *  follows the agent profile: a drone composes its own status, everything else
 *  reads the ground-station composite. */
export function useTelemetry(intervalMs = 400): TelemetryState {
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
        setState({ status, error: null, stale: false });
      } catch (err) {
        if (cancelled || controller.signal.aborted) return;
        setState((prev) => ({
          status: prev.status,
          error: err instanceof Error ? err.message : String(err),
          stale: true,
        }));
      } finally {
        if (!cancelled) {
          timer.current = setTimeout(tick, intervalMs);
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
