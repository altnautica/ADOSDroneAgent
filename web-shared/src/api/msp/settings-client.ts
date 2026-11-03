/**
 * iNav name-based settings client.
 *
 * iNav exposes a name-indexed settings system alongside the traditional
 * virtual-parameter map. The FC maintains a flat list of named settings
 * (e.g. "nav_mc_pos_z_p", "osd_crosshairs"), each with type, range, and
 * a current value encoded as raw bytes.
 *
 * This module wraps the MSP2_COMMON_SETTING, MSP2_COMMON_SET_SETTING, and
 * MSP2_COMMON_SETTING_INFO commands behind a typed async API.
 *
 * @module protocol/msp/settings
 */

import type { MspSerialQueue } from './serial-queue'
import {
  INAV_MSP,
  decodeCommonSetting,
  decodeCommonSettingInfo,
  encodeCommonSetting,
  encodeCommonSetSetting,
  encodeCommonSettingInfo,
  encodeCommonSettingInfoByIndex,
} from './inav-codec'

// ── Setting type codes ────────────────────────────────────────

/**
 * Numeric type codes returned in MSP2_COMMON_SETTING_INFO.
 * Matches `setting_type_e` in iNav firmware `fc/settings.h` (VAR_UINT8..VAR_STRING,
 * 0..6 — there is no INT32; VAR_FLOAT=5, VAR_STRING=6).
 */
export const SettingType = {
  UINT8:  0,
  INT8:   1,
  UINT16: 2,
  INT16:  3,
  UINT32: 4,
  FLOAT:  5,
  STRING: 6,
} as const

export type SettingTypeCode = typeof SettingType[keyof typeof SettingType]

// ── Typed interfaces ──────────────────────────────────────────

/** Metadata about a named setting returned by MSP2_COMMON_SETTING_INFO.
 *  Mirrors the firmware byte layout (name-first; min signed, max unsigned;
 *  enum labels + current value trailing). */
export interface SettingInfo {
  name: string
  pgId: number
  type: number
  section: number
  mode: number
  min: number
  max: number
  index: number
  profileCurrent: number
  profileCount: number
  enumValues?: string[]
  value?: number
}

/** A decoded, typed setting value. */
export type SettingValue =
  | { type: 'uint8';  value: number }
  | { type: 'int8';   value: number }
  | { type: 'uint16'; value: number }
  | { type: 'int16';  value: number }
  | { type: 'uint32'; value: number }
  | { type: 'float';  value: number }
  | { type: 'string'; value: string }
  | { type: 'raw';    value: Uint8Array }

// ── Error ─────────────────────────────────────────────────────

/** Typed error thrown by SettingsClient on protocol failures. */
export class SettingsError extends Error {
  constructor(
    message: string,
    public readonly settingName: string,
    public readonly cause?: unknown,
  ) {
    super(message)
    this.name = 'SettingsError'
  }
}

// ── SettingsClient ────────────────────────────────────────────

/**
 * Typed async client for iNav's name-based settings system.
 *
 * Requires an active `MspSerialQueue` (available while the MSPAdapter is
 * connected). Callers obtain this via `MSPAdapter.settings` after `connect()`.
 *
 * All methods throw `SettingsError` on protocol-level failure.
 */
export class SettingsClient {
  constructor(private readonly queue: MspSerialQueue) {}

  /**
   * Fetch metadata (type, range, profile info) for a named setting.
   * Issues MSP2_COMMON_SETTING_INFO.
   */
  async getInfo(name: string): Promise<SettingInfo> {
    try {
      const payload = encodeCommonSettingInfo(name)
      const frame = await this.queue.send(INAV_MSP.MSP2_COMMON_SETTING_INFO, payload)
      const raw = decodeCommonSettingInfo(new DataView(frame.payload.buffer, frame.payload.byteOffset, frame.payload.byteLength))
      return raw as SettingInfo
    } catch (err) {
      throw new SettingsError(`Failed to get info for setting "${name}"`, name, err)
    }
  }

  /**
   * Fetch a setting's full info BY INDEX — name + type + min/max + mode + enum
   * labels + current value, in a single MSP round-trip (the firmware writes the
   * value trailing the metadata). Throws on a protocol error so the enumeration
   * loop can stop.
   */
  async getInfoByIndex(index: number): Promise<SettingInfo> {
    const payload = encodeCommonSettingInfoByIndex(index)
    const frame = await this.queue.send(INAV_MSP.MSP2_COMMON_SETTING_INFO, payload)
    return decodeCommonSettingInfo(
      new DataView(frame.payload.buffer, frame.payload.byteOffset, frame.payload.byteLength),
    ) as SettingInfo
  }

  /**
   * Enumerate every named setting by requesting indices 0,1,2,… until the FC
   * stops returning a valid named setting (error or empty name). Bounded to
   * guard a misbehaving link.
   */
  async enumerateAllSettings(maxIndex = 2000): Promise<SettingInfo[]> {
    const out: SettingInfo[] = []
    for (let i = 0; i < maxIndex; i++) {
      let info: SettingInfo
      try {
        info = await this.getInfoByIndex(i)
      } catch {
        break // FC returned an error → end of the settings array
      }
      if (!info.name) break // empty name → no more settings
      out.push(info)
    }
    return out
  }

  /**
   * Read the current raw value bytes for a named setting.
   * Issues MSP2_COMMON_SETTING.
   *
   * Returns the raw Uint8Array. Use `getTyped()` for a decoded value.
   */
  async getRaw(name: string): Promise<Uint8Array> {
    try {
      const payload = encodeCommonSetting(name)
      const frame = await this.queue.send(INAV_MSP.MSP2_COMMON_SETTING, payload)
      const result = decodeCommonSetting(new DataView(frame.payload.buffer, frame.payload.byteOffset, frame.payload.byteLength))
      return result.raw
    } catch (err) {
      throw new SettingsError(`Failed to read setting "${name}"`, name, err)
    }
  }

  /**
   * Read and decode a named setting into a typed `SettingValue`.
   * Fetches both the value and its type info in two sequential requests.
   */
  async get(name: string): Promise<SettingValue> {
    const [raw, info] = await Promise.all([this.getRaw(name), this.getInfo(name)])
    return decodeSettingValue(raw, info.type)
  }

  /**
   * Write a new raw-bytes value for a named setting.
   * Issues MSP2_COMMON_SET_SETTING.
   *
   * Use `setTyped()` to encode a typed value automatically.
   */
  async setRaw(name: string, rawValue: Uint8Array): Promise<void> {
    try {
      const payload = encodeCommonSetSetting(name, rawValue)
      await this.queue.send(INAV_MSP.MSP2_COMMON_SET_SETTING, payload)
    } catch (err) {
      throw new SettingsError(`Failed to write setting "${name}"`, name, err)
    }
  }

  /**
   * Write a typed value for a named setting.
   * Fetches the setting type first, then encodes and writes.
   */
  async set(name: string, value: number | string): Promise<void> {
    const info = await this.getInfo(name)
    const raw = encodeSettingValue(value, info.type)
    await this.setRaw(name, raw)
  }

  /**
   * Fetch the list of all parameter group IDs.
   * Issues MSP2_COMMON_PG_LIST.
   */
  async getPgList(): Promise<number[]> {
    try {
      const frame = await this.queue.send(INAV_MSP.MSP2_COMMON_PG_LIST)
      const dv = new DataView(frame.payload.buffer, frame.payload.byteOffset, frame.payload.byteLength)
      const pgIds: number[] = []
      for (let i = 0; i + 1 < dv.byteLength; i += 2) {
        pgIds.push(dv.getUint16(i, true))
      }
      return pgIds
    } catch (err) {
      throw new SettingsError('Failed to fetch PG list', '', err)
    }
  }
}

// ── Value decode/encode helpers ───────────────────────────────

/**
 * Extract a numeric value from a decoded `SettingValue`.
 *
 * Numeric setting types (uint8..float) return their value directly. A string
 * setting parses to a number (NaN → 0); a raw setting reads its first byte.
 * Used by settings panels that edit numeric settings.
 */
export function settingNumber(v: SettingValue): number {
  switch (v.type) {
    case 'string': {
      const n = Number(v.value)
      return Number.isFinite(n) ? n : 0
    }
    case 'raw':
      return v.value.length > 0 ? v.value[0] : 0
    default:
      return v.value
  }
}

function decodeSettingValue(raw: Uint8Array, type: number): SettingValue {
  const dv = new DataView(raw.buffer, raw.byteOffset, raw.byteLength)

  switch (type) {
    case SettingType.UINT8:
      return { type: 'uint8', value: raw.length > 0 ? dv.getUint8(0) : 0 }
    case SettingType.INT8:
      return { type: 'int8', value: raw.length > 0 ? dv.getInt8(0) : 0 }
    case SettingType.UINT16:
      return { type: 'uint16', value: raw.length >= 2 ? dv.getUint16(0, true) : 0 }
    case SettingType.INT16:
      return { type: 'int16', value: raw.length >= 2 ? dv.getInt16(0, true) : 0 }
    case SettingType.UINT32:
      return { type: 'uint32', value: raw.length >= 4 ? dv.getUint32(0, true) : 0 }
    case SettingType.FLOAT:
      return { type: 'float', value: raw.length >= 4 ? dv.getFloat32(0, true) : 0 }
    case SettingType.STRING: {
      // null-terminated ASCII string
      let end = 0
      while (end < raw.length && raw[end] !== 0) end++
      return { type: 'string', value: String.fromCharCode(...raw.subarray(0, end)) }
    }
    default:
      return { type: 'raw', value: raw }
  }
}

function encodeSettingValue(value: number | string, type: number): Uint8Array {
  switch (type) {
    case SettingType.UINT8:
    case SettingType.INT8: {
      return new Uint8Array([Number(value) & 0xff])
    }
    case SettingType.UINT16: {
      const buf = new Uint8Array(2)
      new DataView(buf.buffer).setUint16(0, Number(value), true)
      return buf
    }
    case SettingType.INT16: {
      const buf = new Uint8Array(2)
      new DataView(buf.buffer).setInt16(0, Number(value), true)
      return buf
    }
    case SettingType.UINT32: {
      const buf = new Uint8Array(4)
      new DataView(buf.buffer).setUint32(0, Number(value), true)
      return buf
    }
    case SettingType.FLOAT: {
      const buf = new Uint8Array(4)
      new DataView(buf.buffer).setFloat32(0, Number(value), true)
      return buf
    }
    case SettingType.STRING: {
      const str = String(value)
      const buf = new Uint8Array(str.length + 1)
      for (let i = 0; i < str.length; i++) buf[i] = str.charCodeAt(i)
      buf[str.length] = 0
      return buf
    }
    default: {
      // Pass raw bytes for unknown types; caller can use setRaw() instead
      return new Uint8Array([Number(value) & 0xff])
    }
  }
}
