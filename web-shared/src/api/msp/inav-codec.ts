/**
 * iNav name-based settings codec — the MSP2_COMMON_SETTING family only.
 *
 * A lean subset of the GCS MSP decoder set: the command codes, the little-endian
 * DataView readers, the C-string writer, and the three MSP2_COMMON_* setting
 * decoders/encoders the SettingsClient needs to enumerate and edit iNav's
 * name-indexed settings over the transparent MSP link.
 *
 * @module lib/msp/inav-codec
 * @license GPL-3.0-only
 */

// ── Command codes ─────────────────────────────────────────────

/** MSP2_COMMON_* command codes (name-based settings system). */
export const INAV_MSP = {
  MSP2_COMMON_SETTING: 0x1003,
  MSP2_COMMON_SET_SETTING: 0x1004,
  MSP2_COMMON_SETTING_INFO: 0x1007,
  MSP2_COMMON_PG_LIST: 0x1008,
} as const;

// ── Wire types ────────────────────────────────────────────────

/** MSP2_COMMON_SETTING response — opaque value bytes. */
export interface INavCommonSetting {
  raw: Uint8Array;
}

/** MSP2_COMMON_SETTING_INFO response — a named setting's metadata + value. */
export interface INavSettingInfo {
  name: string;
  pgId: number;
  type: number;
  section: number;
  mode: number;
  min: number;
  max: number;
  index: number;
  profileCurrent: number;
  profileCount: number;
  enumValues?: string[];
  value?: number;
}

// ── Little-endian DataView readers ────────────────────────────

function readU8(dv: DataView, offset: number): number {
  return dv.getUint8(offset);
}
function readU16(dv: DataView, offset: number): number {
  return dv.getUint16(offset, true);
}
function readS16(dv: DataView, offset: number): number {
  return dv.getInt16(offset, true);
}
function readS32(dv: DataView, offset: number): number {
  return dv.getInt32(offset, true);
}
function readU32(dv: DataView, offset: number): number {
  return dv.getUint32(offset, true);
}
function readFloat32(dv: DataView, offset: number): number {
  return dv.getFloat32(offset, true);
}

/** Read a null-terminated ASCII string. Returns `[string, bytesConsumed]`. */
function readCString(dv: DataView, offset: number): [string, number] {
  let end = offset;
  while (end < dv.byteLength && dv.getUint8(end) !== 0) end++;
  const bytes = new Uint8Array(dv.buffer, dv.byteOffset + offset, end - offset);
  const str = String.fromCharCode(...bytes);
  return [str, end - offset + 1]; // +1 for null terminator
}

/** Write a null-terminated ASCII string into `buf` at `offset`; returns bytes written. */
function writeCString(buf: Uint8Array, offset: number, str: string): number {
  for (let i = 0; i < str.length; i++) buf[offset + i] = str.charCodeAt(i) & 0xff;
  buf[offset + str.length] = 0;
  return str.length + 1;
}

// ── Decoders ──────────────────────────────────────────────────

/** MODE_LOOKUP flag in the setting mode byte (firmware setting_mode_e bit 6). */
const MODE_LOOKUP = 1 << 6; // 0x40
/** Defensive cap so a non-lookup setting wrongly flagged can't loop forever. */
const MAX_ENUM_LABELS = 512;

/** Decode the trailing current value by setting type, or undefined if absent. */
function decodeValueAt(dv: DataView, off: number, type: number): number | undefined {
  switch (type) {
    case 0: return off < dv.byteLength ? readU8(dv, off) : undefined;          // UINT8
    case 1: return off < dv.byteLength ? dv.getInt8(off) : undefined;          // INT8
    case 2: return off + 1 < dv.byteLength ? readU16(dv, off) : undefined;     // UINT16
    case 3: return off + 1 < dv.byteLength ? readS16(dv, off) : undefined;     // INT16
    case 4: return off + 3 < dv.byteLength ? readU32(dv, off) : undefined;     // UINT32
    case 5: return off + 3 < dv.byteLength ? readFloat32(dv, off) : undefined; // FLOAT
    default: return undefined;                                                 // STRING / unknown
  }
}

/** MSP2_COMMON_SETTING (0x1003) — raw value bytes; caller interprets by type. */
export function decodeCommonSetting(dv: DataView): INavCommonSetting {
  return { raw: new Uint8Array(dv.buffer, dv.byteOffset, dv.byteLength) };
}

/**
 * MSP2_COMMON_SETTING_INFO (0x1007) — firmware byte layout:
 *   cstring name / U16 pgId / U8 type / U8 section / U8 mode / S32 min / U32 max
 *   / U16 index / U8 profileCurrent / U8 profileCount
 *   / if MODE_LOOKUP: cstring label × (max - min + 1) / value (decoded by type)
 */
export function decodeCommonSettingInfo(dv: DataView): INavSettingInfo {
  const [name, nameLen] = readCString(dv, 0);
  let off = nameLen;
  const pgId = readU16(dv, off); off += 2;
  const type = readU8(dv, off); off += 1;
  const section = readU8(dv, off); off += 1;
  const mode = readU8(dv, off); off += 1;
  const min = readS32(dv, off); off += 4;
  const max = readU32(dv, off); off += 4;
  const index = readU16(dv, off); off += 2;
  const profileCurrent = readU8(dv, off); off += 1;
  const profileCount = readU8(dv, off); off += 1;

  let enumValues: string[] | undefined;
  if ((mode & MODE_LOOKUP) !== 0 && max >= min && max - min < MAX_ENUM_LABELS) {
    enumValues = [];
    for (let i = min; i <= max && off < dv.byteLength; i++) {
      const [label, consumed] = readCString(dv, off);
      enumValues.push(label);
      off += consumed;
    }
  }

  const value = decodeValueAt(dv, off, type);
  return { name, pgId, type, section, mode, min, max, index, profileCurrent, profileCount, enumValues, value };
}

// ── Encoders ──────────────────────────────────────────────────

/** MSP2_COMMON_SETTING request payload — name as a null-terminated string. */
export function encodeCommonSetting(name: string): Uint8Array {
  const buf = new Uint8Array(name.length + 1);
  writeCString(buf, 0, name);
  return buf;
}

/** MSP2_COMMON_SET_SETTING payload — name (null-terminated) then raw value bytes. */
export function encodeCommonSetSetting(name: string, rawValue: Uint8Array): Uint8Array {
  const nameLen = name.length + 1;
  const buf = new Uint8Array(nameLen + rawValue.length);
  writeCString(buf, 0, name);
  buf.set(rawValue, nameLen);
  return buf;
}

/** MSP2_COMMON_SETTING_INFO request payload — name as a null-terminated string. */
export function encodeCommonSettingInfo(name: string): Uint8Array {
  const buf = new Uint8Array(name.length + 1);
  writeCString(buf, 0, name);
  return buf;
}

/**
 * MSP2_COMMON_SETTING_INFO request BY INDEX — a leading 0x00 byte selects
 * "by index" mode, followed by the little-endian uint16 setting index.
 */
export function encodeCommonSettingInfoByIndex(index: number): Uint8Array {
  const buf = new Uint8Array(3);
  buf[0] = 0;
  new DataView(buf.buffer).setUint16(1, index & 0xffff, true);
  return buf;
}
