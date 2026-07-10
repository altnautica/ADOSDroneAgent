/**
 * MSP telemetry command codes + payload decoders for the live Sensors view.
 *
 * The agent is a byte-pipe for MSP (it decodes zero MSP telemetry), so live
 * attitude / battery / GPS / RC / sensor state can only come from the browser
 * running the MSP codec itself over the transparent `ws://<host>:8765/` proxy.
 * These decoders read the little-endian MSP wire layout — NOT the MAVLink one —
 * and are firmware-aware where Betaflight and iNav diverge (sensor-flag bits and
 * the armed indicator).
 *
 * @module lib/msp/telemetry-decoders
 * @license GPL-3.0-only
 */

import type { MspVariant } from "@/lib/fc-firmware";

// ── Command codes ──────────────────────────────────────────────

/** MSP telemetry request codes polled by the live Sensors view. */
export const MSP_CMD = {
  MSP_RC: 105,
  MSP_RAW_GPS: 106,
  MSP_ATTITUDE: 108,
  MSP_ALTITUDE: 109,
  MSP_ANALOG: 110,
  MSP_STATUS_EX: 150,
  /** iNav extended status (MSP2). */
  MSP2_INAV_STATUS: 0x2000,
} as const;

// ── Little-endian DataView readers ─────────────────────────────

const u8 = (dv: DataView, o: number) => dv.getUint8(o);
const u16 = (dv: DataView, o: number) => dv.getUint16(o, true);
const s16 = (dv: DataView, o: number) => dv.getInt16(o, true);
const u32 = (dv: DataView, o: number) => dv.getUint32(o, true);
const s32 = (dv: DataView, o: number) => dv.getInt32(o, true);

function view(payload: Uint8Array): DataView {
  return new DataView(payload.buffer, payload.byteOffset, payload.byteLength);
}

// ── Decoded shapes ─────────────────────────────────────────────

export interface MspAttitude {
  /** degrees */ roll: number;
  /** degrees */ pitch: number;
  /** degrees */ yaw: number;
}

export interface MspAnalog {
  /** volts */ voltage: number;
  /** amps */ amperage: number;
  /** raw 0–1023 */ rssi: number;
  mAhDrawn: number;
}

export interface MspRawGps {
  fixType: number;
  numSat: number;
  lat: number;
  lon: number;
  /** meters */ alt: number;
  /** cm/s */ speed: number;
  /** degrees */ groundCourse: number;
  /** dimensionless (÷100 of the wire value); undefined when the FC omits it. */
  hdop?: number;
}

export interface MspAltitude {
  /** meters */ altitude: number;
  /** cm/s */ vario: number;
}

/** Extended status shared by Betaflight (MSP_STATUS_EX) and iNav (MSP2_INAV_STATUS). */
export interface MspStatus {
  /** microseconds */ cycleTime: number;
  i2cErrors: number;
  /** raw sensor-presence bitmask (firmware-specific layout). */
  sensors: number;
  /** percent (0–100). */ cpuLoad: number;
  armed: boolean;
  /** iNav only: the board reports a hardware fault (sensors bit 15). */
  hardwareFailure?: boolean;
}

export interface SensorFlag {
  label: string;
  present: boolean;
}

// ── Decoders ───────────────────────────────────────────────────

/** MSP_ATTITUDE (108): S16 roll ÷10, S16 pitch ÷10, S16 yaw (whole degrees). */
export function decodeAttitude(payload: Uint8Array): MspAttitude | null {
  if (payload.length < 6) return null;
  const dv = view(payload);
  return { roll: s16(dv, 0) / 10, pitch: s16(dv, 2) / 10, yaw: s16(dv, 4) };
}

/**
 * MSP_ANALOG (110): U8 legacy-voltage, U16 mAhDrawn, U16 rssi, S16 amperage÷100,
 * U16 voltage÷100 (newer, preferred).
 */
export function decodeAnalog(payload: Uint8Array): MspAnalog | null {
  if (payload.length < 7) return null;
  const dv = view(payload);
  const mAhDrawn = u16(dv, 1);
  const rssi = u16(dv, 3);
  const amperage = s16(dv, 5) / 100;
  // Newer U16÷100 voltage at offset 7 when present, else legacy U8÷10 at 0.
  const voltage = payload.length >= 9 ? u16(dv, 7) / 100 : u8(dv, 0) / 10;
  return { voltage, amperage, rssi, mAhDrawn };
}

/**
 * MSP_RAW_GPS (106): U8 fixType, U8 numSat, S32 lat÷1e7, S32 lon÷1e7,
 * U16 alt (m), U16 speed (cm/s), U16 groundCourse÷10, [U16 hdop÷100].
 */
export function decodeRawGps(payload: Uint8Array): MspRawGps | null {
  if (payload.length < 16) return null;
  const dv = view(payload);
  return {
    fixType: u8(dv, 0),
    numSat: u8(dv, 1),
    lat: s32(dv, 2) / 1e7,
    lon: s32(dv, 6) / 1e7,
    alt: u16(dv, 10),
    speed: u16(dv, 12),
    groundCourse: u16(dv, 14) / 10,
    // hdop is appended by newer Betaflight/iNav; absent → left undefined (shown "—").
    hdop: payload.length >= 18 ? u16(dv, 16) / 100 : undefined,
  };
}

/** MSP_ALTITUDE (109): S32 altitude÷100 (m), S16 vario (cm/s) when present. */
export function decodeAltitude(payload: Uint8Array): MspAltitude | null {
  if (payload.length < 4) return null;
  const dv = view(payload);
  return {
    altitude: s32(dv, 0) / 100,
    vario: payload.length >= 6 ? s16(dv, 4) : 0,
  };
}

/** MSP_RC (105): variable-length U16 channels (µs). */
export function decodeRc(payload: Uint8Array): number[] | null {
  if (payload.length < 2) return null;
  const dv = view(payload);
  const channels: number[] = [];
  for (let i = 0; i + 1 < payload.length; i += 2) channels.push(u16(dv, i));
  return channels;
}

/**
 * MSP_STATUS_EX (150) — Betaflight:
 *   U16 cycleTime, U16 i2cErrors, U16 sensors, U32 modeFlags, U8 currentProfile,
 *   U16 cpuLoad, …
 * Armed is bit 0 of modeFlags: Betaflight always emits the ARM box first, so the
 * first flight-mode-flag bit is the ARM state.
 */
export function decodeStatusEx(payload: Uint8Array): MspStatus | null {
  if (payload.length < 13) return null;
  const dv = view(payload);
  const cycleTime = u16(dv, 0);
  const i2cErrors = u16(dv, 2);
  const sensors = u16(dv, 4);
  const modeFlags = u32(dv, 6);
  const cpuLoad = u16(dv, 11);
  return {
    cycleTime,
    i2cErrors,
    sensors,
    cpuLoad,
    armed: (modeFlags & 0x1) !== 0,
  };
}

/**
 * MSP2_INAV_STATUS (0x2000) — iNav:
 *   U16 cycleTime, U16 i2cErrors, U16 sensors, U16 reserved, U32 modeFlags,
 *   U8 currentProfile, U16 cpuLoad, U8 profileCount, U8 rateProfile,
 *   U32 armingFlags, …
 * Armed is bit 2 of armingFlags; hardware-fault is sensors bit 15.
 */
export function decodeInavStatus(payload: Uint8Array): MspStatus | null {
  if (payload.length < 21) return null;
  const dv = view(payload);
  const cycleTime = u16(dv, 0);
  const i2cErrors = u16(dv, 2);
  const sensors = u16(dv, 4);
  const cpuLoad = u16(dv, 13);
  const armingFlags = u32(dv, 17);
  return {
    cycleTime,
    i2cErrors,
    sensors,
    cpuLoad,
    armed: (armingFlags & (1 << 2)) !== 0,
    hardwareFailure: (sensors & (1 << 15)) !== 0,
  };
}

/**
 * Decode the sensor-presence bitmask into labelled flags. Betaflight and iNav
 * pack different bits (Betaflight bit 5 = gyro; iNav bit 5 = optical flow,
 * bit 6 = pitot), so the decode is firmware-specific. Only bits sourced exactly
 * from firmware are surfaced — never a guessed label.
 */
export function decodeSensorFlags(sensors: number, firmware: MspVariant): SensorFlag[] {
  const bit = (n: number) => (sensors & (1 << n)) !== 0;
  if (firmware === "inav") {
    return [
      { label: "Accel", present: bit(0) },
      { label: "Baro", present: bit(1) },
      { label: "Mag", present: bit(2) },
      { label: "GPS", present: bit(3) },
      { label: "Rangefinder", present: bit(4) },
      { label: "Optical flow", present: bit(5) },
      { label: "Pitot", present: bit(6) },
    ];
  }
  // Betaflight
  return [
    { label: "Gyro", present: bit(5) },
    { label: "Accel", present: bit(0) },
    { label: "Baro", present: bit(1) },
    { label: "Mag", present: bit(2) },
    { label: "GPS", present: bit(3) },
    { label: "Sonar", present: bit(4) },
  ];
}

/** MSP GPS fixType → short human label. */
export function gpsFixLabel(fixType: number): string {
  switch (fixType) {
    case 0:
      return "no fix";
    case 1:
      return "2D";
    case 2:
      return "3D";
    default:
      return `fix ${fixType}`;
  }
}
