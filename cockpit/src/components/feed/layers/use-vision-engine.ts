// One shared `GET /api/vision/status` poll (every 2 s) while anything on
// screen needs the vision engine's state: the Vision panel's engine card, or a
// vision layer's status chip. The answer goes into the layer chips store, so
// every reader sees the same read and the agent sees one poll however many
// readers are mounted. Demo mode reads a running mock engine and never polls.
// A ground station runs no vision engine of its own (the route is a 503
// there), so it never polls; nor does any node before its profile is known.

import { useEffect } from "react";

import { isDemoMode } from "@/lib/utils";
import { apiFetch } from "@/shared/api-fetch";
import { useProfile } from "@/shared/use-profile";
import { useLayerChipsStore, type VisionEngineStatus } from "@/stores/layer-chips-store";

export const VISION_POLL_MS = 2000;

const DEMO_STATUS: VisionEngineStatus = { backend: "mock", backend_state: "mock", backend_reason: null, models: [] };

let readers = 0;
let timer: number | undefined;
let aborter: AbortController | null = null;

/** Read the status now (also after a change the operator made). */
export function pollVisionNow(): void {
  if (isDemoMode()) {
    useLayerChipsStore.getState().setVision(DEMO_STATUS, false);
    return;
  }
  const signal = aborter?.signal;
  apiFetch<VisionEngineStatus>("/api/vision/status", { signal }).then(
    (s) => useLayerChipsStore.getState().setVision(s, false),
    () => {
      if (!signal?.aborted) useLayerChipsStore.getState().setVision(null, true);
    },
  );
}

/** Keep the shared poll running while `active`, on any node but a ground
 *  station. */
export function useVisionEngine(active: boolean): void {
  const profile = useProfile();
  const on = active && (isDemoMode() || (profile !== null && profile !== "ground_station"));
  useEffect(() => {
    if (!on) return;
    readers++;
    if (readers === 1) {
      aborter = new AbortController();
      pollVisionNow();
      if (!isDemoMode()) timer = window.setInterval(pollVisionNow, VISION_POLL_MS);
    }
    return () => {
      readers--;
      if (readers > 0) return;
      clearInterval(timer);
      timer = undefined;
      aborter?.abort();
      aborter = null;
    };
  }, [on]);
}
