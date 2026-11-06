// Holds the vision engine's detection lease on the camera a target overlay
// shows, for as long as that overlay is mounted and `enabled`; a camera change
// releases the old camera's lease and takes the new one. Off (null state) in
// demo mode, where a synthetic stream feeds the overlay, and wherever the
// detections come from another node's engine (a ground station).

import { useEffect, useState } from "react";

import { requestDetection } from "@/lib/api";
import { startDetectionLease, type DetectorState } from "@/lib/detection-lease";
import { isDemoMode } from "@/lib/utils";

export function useDetectionLease(cameraId: string | null, enabled: boolean): DetectorState | null {
  const [state, setState] = useState<{ camera: string; state: DetectorState } | null>(null);
  const active = enabled && cameraId !== null && !isDemoMode();

  useEffect(() => {
    if (!active || cameraId === null) return;
    return startDetectionLease(
      cameraId,
      (camera, holdS) => requestDetection(camera, holdS),
      (s) => setState({ camera: cameraId, state: s }),
    );
  }, [active, cameraId]);

  // A state left over from another camera, or from before the lease went off,
  // says nothing about this one.
  return active && state?.camera === cameraId ? state.state : null;
}
