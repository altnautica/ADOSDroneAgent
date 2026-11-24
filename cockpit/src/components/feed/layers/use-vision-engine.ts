// One shared `GET /api/vision/status` poll (every 2 s) while anything on
// screen needs the vision engine's state: the Vision panel's engine card, or a
// vision layer's status chip. The answer goes into the layer chips store, so
// every reader sees the same read and the agent sees one poll however many
// readers are mounted. Demo mode reads a running mock engine and never polls.

import { useEffect } from "react";

import { isDemoMode } from "@/lib/utils";
import { apiFetch } from "@/shared/api-fetch";
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

/** Keep the shared poll running while `active`. */
export function useVisionEngine(active: boolean): void {
  useEffect(() => {
    if (!active) return;
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
  }, [active]);
}
