/**
 * Resolve the parameter-metadata dispatch key from the agent's FC identity.
 *
 * The agent reports the canonical firmware family on `/api/status` (`fcFirmware`)
 * and the vehicle on the dashboard snapshot (`fc.vehicle`); together they select
 * the right metadata catalog (ArduPilot splits per vehicle).
 *
 * @module lib/fc-firmware
 */

import type { FirmwareType } from "./param-metadata";

export function resolveFirmwareType(fcFirmware?: string | null, vehicle?: string | null): FirmwareType {
  switch ((fcFirmware ?? "").toLowerCase()) {
    case "px4": return "px4";
    case "betaflight": return "betaflight";
    case "inav": return "inav";
    case "ardupilot": {
      const v = (vehicle ?? "").toLowerCase();
      if (v.includes("plane")) return "ardupilot-plane";
      if (v.includes("rover") || v.includes("boat")) return "ardupilot-rover";
      if (v.includes("sub")) return "ardupilot-sub";
      return "ardupilot-copter"; // default when the vehicle isn't known yet
    }
    default: return "unknown";
  }
}
