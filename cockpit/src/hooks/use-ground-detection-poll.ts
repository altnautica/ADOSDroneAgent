// Runs the ground-station detection poll for the Feed's lifetime. A ground
// node has no local vision engine, so its boxes must be received over the radio
// from the linked drone rather than read off a local socket. No-ops on any
// profile other than a ground station, and on a ground station with no linked
// drone yet (no relay peer) — the overlay simply shows clean no-signal.

import { useEffect } from "react";

import { connectGroundDetectionPoll } from "@/lib/vision-detections-poll";
import { useProfile } from "@/hooks/use-profile";
import { useTelemetryContext } from "@/hooks/telemetry-context";

export function useGroundDetectionPoll(): void {
  const profile = useProfile();
  const { status } = useTelemetryContext();
  const isGround = profile === "ground_station";
  // The linked drone: the radio pair's device id. Null until the radio is
  // bound, so no relay call is made before there is a peer to reach.
  const peer = isGround ? (status?.paired_drone?.device_id ?? null) : null;

  useEffect(() => {
    if (!peer) return;
    const stop = connectGroundDetectionPoll({ peer });
    return stop;
  }, [peer]);
}
