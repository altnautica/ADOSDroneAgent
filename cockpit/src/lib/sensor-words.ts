// The navigation sensor words the Feed shows (cockpit spec
// `sensor-health.json`): flow and range beside the bottom row's AGL, the
// lidar's scan rate in the radar's header, both from the pilot kind's
// `sensors` while the pilot frame is fresh, and a stream leg's camera state
// from the sensors route's `health.cameras`.

import type { PilotState } from "@/shared/hud/cockpit-frames";
import { pilotFresh } from "@/shared/hud/pilot-fresh";
import {
  cameraForLeg,
  cameraTabWord,
  lidarWord,
  parseSensorHealth,
  type CameraHealth,
  type SensorHealth,
  type SensorWord,
} from "@/shared/hud/sensor-health";

type StampedPilot = { data: Pick<PilotState, "sensors">; at: number } | null;

/** The pilot's `sensors` while its frame is fresh; null with no fresh frame
 *  or no block (no FC link). */
export function freshSensors(pilot: StampedPilot, nowMs: number): SensorHealth | null {
  return pilot && pilotFresh(pilot.at, nowMs) ? parseSensorHealth(pilot.data.sensors) : null;
}

/** The radar header: the lidar's word (`Lidar 9.9 Hz`, `Lidar lost`, `No
 *  lidar`) from the pilot's `sensors.lidar`; null without fresh sensors. */
export function radarLidarWord(pilot: StampedPilot, nowMs: number): SensorWord | null {
  const s = freshSensors(pilot, nowMs);
  return s ? lidarWord(s.lidar) : null;
}

/** A stream leg's camera word (stale or lost only), named by its label. */
export function legCameraWord(
  cameras: readonly CameraHealth[],
  leg: { id: string; role?: string | null },
  label: string,
): SensorWord | null {
  return cameraTabWord(cameraForLeg(cameras, leg), label);
}
