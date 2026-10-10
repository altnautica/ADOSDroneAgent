// Runs the ground-station detection poll for the Feed's lifetime. A ground
// node has no local vision engine, so its boxes are received over the radio
// from the linked drone. No-op on any other profile, and on a ground station
// with no linked drone yet (no relay peer).

import { useEffect } from "react";

import { connectGroundDetectionPoll } from "@/lib/vision-detections-poll";
import { useProfile } from "@/shared/use-profile";
import { useStatusStore } from "@/stores/status-store";

export function useGroundDetectionPoll(): void {
  const isGround = useProfile() === "ground_station";
  // The radio pair's device id; null until the radio is bound.
  const peer = useStatusStore((s) => (isGround ? (s.status?.paired_drone?.device_id ?? null) : null));

  useEffect(() => {
    if (!peer) return;
    return connectGroundDetectionPoll({ peer });
  }, [peer]);
}
