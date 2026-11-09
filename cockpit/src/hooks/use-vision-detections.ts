// Runs the on-box detection feed for the Feed's lifetime: opens the live vision
// WebSocket (or, in demo mode, the synthetic mock stream) and clears it on
// unmount. Mounted inside the Feed so the socket runs only while the flying view
// is on screen, and only on a node with a local vision engine — a ground
// station has no local camera/engine, so its detections would arrive over a
// different path (not this local socket) and enabling it there would only churn
// reconnects against an absent socket.

import { useEffect } from "react";

import { connectVisionDetections } from "@/lib/vision-detections-ws";
import { mockDetectionStream } from "@/lib/mock-detections";
import { isDemoMode } from "@/lib/utils";

/** What decides whether this node has a local vision engine. */
export interface LocalVisionInputs {
  /** The node owns its cameras (a drone or a visor). */
  ownsCameras: boolean;
  /** The status `perception_tier` (`none` when the board declares no path). */
  perceptionTier: string | null;
  /** Whether the status services list shows the vision engine running. */
  visionEngine: boolean | null;
  profile: string | null;
}

/**
 * Whether this node has a vision engine whose detections the Feed should
 * stream: a camera node whose board declares a vision path, or whose engine is
 * running anyway (a board without a declared path where a backend was
 * configured; its tier still reads `none`), and a workstation or compute node.
 * A camera node in demo mode runs the synthetic detector, so the demo draws
 * its boxes with no agent status to read.
 */
export function hasLocalVision({ ownsCameras, perceptionTier, visionEngine, profile }: LocalVisionInputs): boolean {
  const declared = perceptionTier !== null && perceptionTier !== "none";
  return (ownsCameras && (declared || visionEngine === true || isDemoMode())) || profile === "workstation" || profile === "compute";
}

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
