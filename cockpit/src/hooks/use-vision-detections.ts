// Runs the on-box detection feed for the Feed's lifetime: opens the live vision
// WebSocket (or, in demo mode, the synthetic mock stream) and clears it on
// unmount. Mounted inside the Feed so the socket runs only while the flying view
// is on screen, and only on a companion node that can run a vision engine — a
// ground station has no local camera/engine, so its detections would arrive over
// a different path (not this local socket) and enabling it there would only churn
// reconnects against an absent socket.

import { useEffect } from "react";

import { connectVisionDetections } from "@/lib/vision-detections-ws";
import { mockDetectionStream } from "@/lib/mock-detections";
import { isDemoMode } from "@/lib/utils";

/**
 * Drive the detection store while `enabled`. In demo mode a synthetic stream
 * feeds the store; otherwise the live WebSocket to this box's own agent does.
 * Either way the store is cleared on teardown so stale boxes never linger.
 */
export function useVisionDetections(enabled: boolean): void {
  useEffect(() => {
    if (!enabled) return;

    if (isDemoMode()) {
      mockDetectionStream.start();
      return () => mockDetectionStream.stop();
    }

    const stop = connectVisionDetections();
    return stop;
  }, [enabled]);
}
