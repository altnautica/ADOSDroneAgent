// `GET /api/telemetry`: the router's live vehicle snapshot (attitude in
// radians, battery, GPS, position). Every field is absent or null until the FC
// reports it; nothing here is defaulted. One cache entry for every reader.

import { useResource } from "@/hooks/use-resource";

export const VEHICLE_TELEMETRY_POLL_MS = 1_000;

export interface VehicleTelemetry {
  armed?: boolean | null;
  mode?: string | null;
  attitude?: { roll?: number | null; pitch?: number | null; yaw?: number | null };
  battery?: { voltage?: number | null; current?: number | null; remaining?: number | null };
  gps?: { fix_type?: number | null; satellites?: number | null; eph?: number | null };
  position?: { lat?: number | null; lon?: number | null; alt_rel?: number | null };
  /** Distance to home (m), when the agent supplies it. */
  home_distance?: number | null;
  /** The FC's home position, absent until it reported one. */
  home?: { lat_e7?: number | null; lon_e7?: number | null } | null;
  /** LOCAL_POSITION_NED (m) and its age, null until the FC sent one. The
   *  agent passes it through until it is 5 s old. */
  local_position?: { x: number; y: number; age_ms?: number | null } | null;
  /** The vehicle's own stamps; one moving says the readings are current. */
  last_update?: string | null;
  last_heartbeat?: string | null;
  /** The FC's PreArm/Arm STATUSTEXT from the last 10 s; absent while the FC link is down. */
  prearm?: string[];
}

export function useVehicleTelemetry() {
  return useResource<VehicleTelemetry>("vehicle-telemetry", "/api/telemetry", VEHICLE_TELEMETRY_POLL_MS);
}

/** MAVLink GPS_RAW_INT reports an unknown HDOP as 65535 (655.35 after the
 *  router's /100); the snapshot path keeps only 0 < hdop < 655.35, so every
 *  path does. */
export function hdopOrNull(eph: number | null | undefined): number | null {
  return typeof eph === "number" && eph > 0 && eph < 655.35 ? eph : null;
}
