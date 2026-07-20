// Polls the composite ground-station status at 2–5 Hz and hands the latest
// snapshot to the status strip + screens. On a failed poll it keeps the last
// snapshot and flips `stale` so surfaces can dim honestly rather than blank or
// fabricate. Uses a self-scheduling timeout so a slow poll never
// overlaps the next.

import { useEffect, useRef, useState } from "react";

import { getGsStatus } from "@/lib/api";
import type { GsStatus } from "@/lib/types";

export interface TelemetryState {
  status: GsStatus | null;
  error: string | null;
  /** True when the most recent poll failed (the snapshot may be old). */
  stale: boolean;
}

/** Poll GS status every `intervalMs` (default 400 ms ≈ 2.5 Hz). */
export function useTelemetry(intervalMs = 400): TelemetryState {
  const [state, setState] = useState<TelemetryState>({
    status: null,
    error: null,
    stale: false,
  });
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    let cancelled = false;
    const controller = new AbortController();

    const tick = async () => {
      try {
        const status = await getGsStatus(controller.signal);
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
  }, [intervalMs]);

  return state;
}
