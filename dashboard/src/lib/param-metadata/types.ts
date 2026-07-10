/**
 * Parameter metadata types — the cross-firmware superset shape.
 *
 * One lossless shape covers ArduPilot, PX4, iNav, and Betaflight parameter
 * definitions (enum values, bitmask flags, ranges, units, defaults, and the
 * advisory flags each firmware exposes). Consumers read this shape regardless
 * of which firmware the connected vehicle runs.
 *
 * @module protocol/param-metadata/types
 * @license GPL-3.0-only
 */

/** Storage/value type for a parameter. ArduPilot params travel as float over
 *  MAVLink but are typed in metadata; PX4/iNav/Betaflight expose the real type. */
export type ParamValueType =
  | "uint8" | "int8" | "uint16" | "int16" | "uint32" | "int32"
  | "float" | "string" | "bool";

/** Provenance for a generated snapshot — drives verification + drift detection. */
export interface ParamSnapshotProvenance {
  firmware: string;
  version: string;
  sourceUrl?: string;
  generatedAt?: string;
  paramCount: number;
}

export interface ParamMetadata {
  name: string;
  humanName: string;
  description: string;
  range?: { min: number; max: number };
  units?: string;
  /** Enum code → label. Codes may be non-integer (e.g. ArduPilot 0.1 "Very Low"). */
  values?: Map<number, string>;
  /** Bit index → label. */
  bitmask?: Map<number, string>;
  /** Optional per-bit long description (PX4 carries these). */
  bitmaskDescriptions?: Map<number, string>;
  increment?: number;
  defaultValue?: number;
  rebootRequired?: boolean;
  // ── advisory flags (additive, optional) ──
  /** ArduPilot User=Standard|Advanced; inferred elsewhere. */
  advanced?: boolean;
  readOnly?: boolean;
  /** Changes at runtime — do not cache/persist as a setpoint. */
  volatile?: boolean;
  /** Calibration-only parameter (ArduPilot). */
  calibration?: boolean;
  /** Member of a 3-component vector family (ArduPilot). */
  vector3?: boolean;
  /** Storage type for write-range validation. */
  valueType?: ParamValueType;
  /** Grouping for organization/filtering (PX4 category, AP library prefix, iNav PG). */
  category?: string;
  group?: string;
  /** Display precision (PX4 decimal). */
  decimalPlaces?: number;
}

/** A bundled/hosted snapshot file: provenance + the serialized params. */
export interface ParamSnapshot {
  provenance: ParamSnapshotProvenance;
  params: SerializedMeta[];
}

/** JSON-safe form of ParamMetadata (Maps → entry arrays). Shared by the bundled
 *  files, the hosted blobs, and the IndexedDB cache. */
export type SerializedMeta =
  Omit<ParamMetadata, "values" | "bitmask" | "bitmaskDescriptions"> & {
    values?: [number, string][];
    bitmask?: [number, string][];
    bitmaskDescriptions?: [number, string][];
  };

export function serializeMeta(meta: ParamMetadata): SerializedMeta {
  return {
    ...meta,
    values: meta.values ? Array.from(meta.values.entries()) : undefined,
    bitmask: meta.bitmask ? Array.from(meta.bitmask.entries()) : undefined,
    bitmaskDescriptions: meta.bitmaskDescriptions
      ? Array.from(meta.bitmaskDescriptions.entries())
      : undefined,
  };
}

export function deserializeMeta(s: SerializedMeta): ParamMetadata {
  return {
    ...s,
    values: s.values ? new Map(s.values) : undefined,
    bitmask: s.bitmask ? new Map(s.bitmask) : undefined,
    bitmaskDescriptions: s.bitmaskDescriptions
      ? new Map(s.bitmaskDescriptions)
      : undefined,
  };
}

export function deserializeMetaMap(arr: SerializedMeta[]): Map<string, ParamMetadata> {
  const map = new Map<string, ParamMetadata>();
  for (const s of arr) map.set(s.name, deserializeMeta(s));
  return map;
}

/** ArduPilot vehicle directory name (retained for back-compat with param-docs). */
export type ArduPilotVehicle = "ArduCopter" | "ArduPlane" | "Rover" | "ArduSub";

/** Firmware family + vehicle, the metadata dispatch key. */
export type FirmwareType =
  | "ardupilot-copter" | "ardupilot-plane" | "ardupilot-rover" | "ardupilot-sub"
  | "px4" | "inav" | "betaflight" | "unknown";
