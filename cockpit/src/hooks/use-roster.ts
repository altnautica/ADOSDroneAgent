// The camera roster (`GET /api/video/roster`), polled every 30 s while the Feed
// is up: cameras change rarely, and the concurrent legs already ride the 1 Hz
// status body. Only a drone has cameras; a ground station's roster is empty,
// so the poll is not run there. A failed poll keeps the last list.

import { useEffect, useState } from "react";

import { getRoster } from "@/lib/api";
import type { RosterCamera } from "@/lib/types";
import { pollLoop } from "@/hooks/use-status-poll";

export const ROSTER_POLL_MS = 30_000;

export function useRoster(enabled: boolean): RosterCamera[] {
  const [cameras, setCameras] = useState<RosterCamera[]>([]);

  useEffect(() => {
    if (!enabled) return;
    return pollLoop(
      async (signal) => {
        try {
          const res = await getRoster(signal);
          setCameras(Array.isArray(res?.cameras) ? res.cameras : []);
        } catch {
          // Keep the last list: a slow, non-critical read.
        }
      },
      () => ROSTER_POLL_MS,
    );
  }, [enabled]);

  return cameras;
}
