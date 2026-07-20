// Polls the camera roster (`GET /api/video/roster`) slowly — cameras change
// rarely, so a 5 s cadence is plenty. The Feed uses the result only to decide
// whether to show multi-stream tabs (more than one camera). A failed poll keeps
// the last list; a ground station returns an empty list, so the tabs stay
// hidden there.

import { useEffect, useRef, useState } from "react";

import { getRoster } from "@/lib/api";
import type { RosterCamera } from "@/lib/types";

export function useRoster(intervalMs = 5000): RosterCamera[] {
  const [cameras, setCameras] = useState<RosterCamera[]>([]);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    let cancelled = false;
    const controller = new AbortController();

    const tick = async () => {
      try {
        const res = await getRoster(controller.signal);
        if (cancelled) return;
        setCameras(Array.isArray(res?.cameras) ? res.cameras : []);
      } catch {
        // keep the last list — the roster is a slow, non-critical read
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

  return cameras;
}
