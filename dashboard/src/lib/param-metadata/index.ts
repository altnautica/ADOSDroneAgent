/**
 * Parameter metadata provider (dashboard) — the offline bundled floor.
 *
 * Loads the committed `public/param-metadata/<firmware>.json.gz` catalog for a
 * firmware (and, for Betaflight, the version-keyed CLI-settings catalog merged
 * with it), so the parameter editor renders enum dropdowns / bitmask editors /
 * ranged numerics offline. The dashboard omits the GCS hosted + live-FC overlay
 * tiers — the bundled floor is the source. Always resolves; never throws.
 *
 * @module lib/param-metadata
 * @license GPL-3.0-only
 */

import type { FirmwareType, ParamMetadata } from "./types";
import { loadBundled } from "./bundled";
import { loadBfSettingsMetadata } from "./bf-settings";

export type { FirmwareType, ParamMetadata, ParamValueType } from "./types";
export { loadBfSettingsMetadata } from "./bf-settings";

export interface ParamMetadataQuery {
  firmwareType: FirmwareType;
  /** Major.minor or full tag; selects the Betaflight version-matched catalog. */
  firmwareVersion?: string | null;
}

/**
 * Load parameter metadata for a firmware. Betaflight merges its version-keyed
 * CLI-settings catalog with the curated floor; every other firmware uses the
 * bundled floor directly.
 */
export async function loadParamMetadata(q: ParamMetadataQuery): Promise<Map<string, ParamMetadata>> {
  if (q.firmwareType === "betaflight") return loadBfSettingsMetadata(q.firmwareVersion ?? undefined);
  return loadBundled(q.firmwareType);
}

/** Map an ArduPilot firmwareType to its vehicle directory name. */
export function firmwareTypeToVehicle(
  ft: FirmwareType,
): "ArduCopter" | "ArduPlane" | "Rover" | "ArduSub" | null {
  switch (ft) {
    case "ardupilot-copter": return "ArduCopter";
    case "ardupilot-plane": return "ArduPlane";
    case "ardupilot-rover": return "Rover";
    case "ardupilot-sub": return "ArduSub";
    default: return null;
  }
}

