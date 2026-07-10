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

/** MSP FC variants the agent identifies over the transparent serial link. These
 *  never emit a MAVLink heartbeat, so `fc.connected` stays false while the agent
 *  reports the variant on `/api/status`. */
export type MspVariant = "betaflight" | "inav";

/** Narrow a heartbeat's `fcVariant` to a known MSP firmware, or null. */
export function mspVariant(fcVariant?: string | null): MspVariant | null {
  switch ((fcVariant ?? "").toLowerCase()) {
    case "betaflight":
      return "betaflight";
    case "inav":
      return "inav";
    default:
      return null;
  }
}

/** Human label for an identified MSP FC, e.g. "Betaflight (MSP)" / "iNav (MSP)". */
export function mspVariantLabel(variant: MspVariant): string {
  return variant === "betaflight" ? "Betaflight (MSP)" : "iNav (MSP)";
}

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
