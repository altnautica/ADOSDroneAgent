// The cameras' health (`GET /api/v1/sensors` `health.cameras`), polled every
// 2 s while the Feed is up on a node with cameras, for the stale / lost words
// on the stream tabs and the picture-in-picture label. A failed read clears
// the list: an unknown camera state shows no word rather than an old one.

import { useEffect } from "react";
import { create } from "zustand";

import { getSensors } from "@/lib/sensors-model";
import { pollLoop } from "@/hooks/use-status-poll";
import { parseSensorsHealth, type CameraHealth } from "@/shared/hud/sensor-health";

export const CAMERA_HEALTH_POLL_MS = 2000;

interface CameraHealthState {
  cameras: readonly CameraHealth[];
}

export const useCameraHealthStore = create<CameraHealthState>(() => ({ cameras: [] }));

export function useCameraHealthPoll(enabled: boolean): void {
  useEffect(() => {
    if (!enabled) return;
    const stop = pollLoop(
      async (signal) => {
        try {
          const body = await getSensors(signal);
          const health = "health" in body ? body.health : null;
          useCameraHealthStore.setState({ cameras: parseSensorsHealth(health)?.cameras ?? [] });
        } catch {
          useCameraHealthStore.setState({ cameras: [] });
        }
      },
      () => CAMERA_HEALTH_POLL_MS,
    );
    return () => {
      stop();
      useCameraHealthStore.setState({ cameras: [] });
    };
  }, [enabled]);
}
