/**
 * MSPv1 and MSPv2 frame encoder/decoder.
 *
 * MSPv1: $M< (request) / $M> (response) / $M! (error)
 *   Header: '$' 'M' dir(1) len(1) cmd(1) payload(N) xor_checksum(1)
 *   Jumbo:  len=255 triggers 2-byte length after cmd byte
 *
 * MSPv2: $X< (request) / $X> (response) / $X! (error)
 *   Header: '$' 'X' dir(1) flags(1) cmd(2 LE) len(2 LE) payload(N) crc8(1)
 *
 * @module protocol/msp/msp-codec
 */

// ── Types ──────────────────────────────────────────────────

export interface MspFrame {
  version: 1 | 2;
  command: number;
  payload: Uint8Array;
  direction: 'request' | 'response' | 'error';
}

// ── Protocol bytes ─────────────────────────────────────────

const DOLLAR = 0x24; // '$'
const M_CHAR = 0x4d; // 'M'
const X_CHAR = 0x58; // 'X'
const DIR_TO_FC = 0x3c; // '<'
const DIR_FROM_FC = 0x3e; // '>'
const DIR_ERROR = 0x21; // '!'

/** MSPv1 jumbo frame threshold. Payloads of exactly 255 bytes use jumbo framing. */
const JUMBO_FRAME_MIN_SIZE = 255;

// ── CRC Functions ──────────────────────────────────────────

/**
 * CRC8 DVB-S2 (polynomial 0xD5) used by MSPv2.
 * Processes bytes from `start` to `end` (exclusive) in `buf`.
 * If called with no range, processes entire buffer.
 */
export function crc8DvbS2(buf: Uint8Array, start = 0, end = buf.length): number {
  let crc = 0;
  for (let i = start; i < end; i++) {
    crc ^= buf[i];
    for (let j = 0; j < 8; j++) {
      if (crc & 0x80) {
        crc = ((crc << 1) & 0xff) ^ 0xd5;
      } else {
        crc = (crc << 1) & 0xff;
      }
    }
  }
  return crc;
}

/**
 * Single-byte CRC8 DVB-S2 update. Folds one byte into an existing CRC.
 */
export function crc8DvbS2Update(crc: number, byte: number): number {
  crc ^= byte;
  for (let j = 0; j < 8; j++) {
    if (crc & 0x80) {
      crc = ((crc << 1) & 0xff) ^ 0xd5;
    } else {
      crc = (crc << 1) & 0xff;
    }
  }
  return crc;
}

/**
 * XOR checksum used by MSPv1.
 * XOR of size byte, command byte, and each payload byte.
 * For jumbo frames, size byte is 255 and the real length is XORed as 2 additional bytes.
 */
export function xorChecksum(size: number, cmd: number, payload: Uint8Array): number {
  let crc: number;
  if (size >= JUMBO_FRAME_MIN_SIZE) {
    // Jumbo: size field is 255, then XOR in the real 16-bit length
    crc = JUMBO_FRAME_MIN_SIZE ^ cmd;
    crc ^= size & 0xff;
    crc ^= (size >> 8) & 0xff;
  } else {
    crc = size ^ cmd;
  }
  for (let i = 0; i < payload.length; i++) {
    crc ^= payload[i];
  }
  return crc & 0xff;
}

// ── Encoders ───────────────────────────────────────────────

/**
 * Encode an MSPv1 request frame.
 *
 * Standard frame (payload < 255 bytes):
 *   $M< + len(1) + cmd(1) + payload(N) + xor(1)
 *   Total: 6 + N bytes
 *
 * Jumbo frame (payload >= 255 bytes):
 *   $M< + 255(1) + cmd(1) + realLen_low(1) + realLen_high(1) + payload(N) + xor(1)
 *   Total: 8 + N bytes
 */
export function encodeMspV1(command: number, payload?: Uint8Array): Uint8Array {
  const data = payload ?? new Uint8Array(0);
  const dataLength = data.length;
  const isJumbo = dataLength >= JUMBO_FRAME_MIN_SIZE;

  const overhead = isJumbo ? 8 : 6;
  const buf = new Uint8Array(overhead + dataLength);

  buf[0] = DOLLAR;
  buf[1] = M_CHAR;
  buf[2] = DIR_TO_FC;

  if (isJumbo) {
    buf[3] = JUMBO_FRAME_MIN_SIZE;
    buf[4] = command & 0xff;
    buf[5] = dataLength & 0xff;
    buf[6] = (dataLength >> 8) & 0xff;
    buf.set(data, 7);
  } else {
    buf[3] = dataLength;
    buf[4] = command & 0xff;
    buf.set(data, 5);
  }

  buf[buf.length - 1] = xorChecksum(dataLength, command, data);
  return buf;
}

/**
 * Encode an MSPv2 request frame.
 *
 *   $X< + flags(1) + cmd_low(1) + cmd_high(1) + len_low(1) + len_high(1) + payload(N) + crc8(1)
 *   Total: 9 + N bytes
 *
 * CRC8 covers bytes [3..end-1] (flags through last payload byte).
 */
export function encodeMspV2(command: number, payload?: Uint8Array): Uint8Array {
  const data = payload ?? new Uint8Array(0);
  const dataLength = data.length;
  const bufferSize = 9 + dataLength;
  const buf = new Uint8Array(bufferSize);

  buf[0] = DOLLAR;
  buf[1] = X_CHAR;
  buf[2] = DIR_TO_FC;
  buf[3] = 0; // flags
  buf[4] = command & 0xff;
  buf[5] = (command >> 8) & 0xff;
  buf[6] = dataLength & 0xff;
  buf[7] = (dataLength >> 8) & 0xff;
  buf.set(data, 8);

  // CRC covers flags + cmd + len + payload (indices 3 through bufferSize-2)
  buf[bufferSize - 1] = crc8DvbS2(buf, 3, bufferSize - 1);

  return buf;
}

/**
 * Smart encoder: use V1 for commands that fit (code < 255 and payload < 255 bytes),
 * V2 otherwise (MSP2 codes or large payloads).
 */
export function encodeMsp(command: number, payload?: Uint8Array): Uint8Array {
  const data = payload ?? new Uint8Array(0);
  if (command <= 254 && data.length < JUMBO_FRAME_MIN_SIZE) {
    return encodeMspV1(command, data);
  }
  return encodeMspV2(command, data);
}

// ── Direction byte helpers ─────────────────────────────────

export function directionByte(dir: MspFrame['direction'], version: 1 | 2): number {
  const proto = version === 1 ? M_CHAR : X_CHAR;
  let dirChar: number;
  switch (dir) {
    case 'request': dirChar = DIR_TO_FC; break;
    case 'response': dirChar = DIR_FROM_FC; break;
    case 'error': dirChar = DIR_ERROR; break;
  }
  // Return is just the direction char; proto is separate.
  void proto;
  return dirChar;
}

/**
 * Encode a response or error frame (useful for testing and SITL bridges).
 */
export function encodeResponseV1(command: number, payload: Uint8Array, isError = false): Uint8Array {
  const dataLength = payload.length;
  const isJumbo = dataLength >= JUMBO_FRAME_MIN_SIZE;
  const overhead = isJumbo ? 8 : 6;
  const buf = new Uint8Array(overhead + dataLength);

  buf[0] = DOLLAR;
  buf[1] = M_CHAR;
  buf[2] = isError ? DIR_ERROR : DIR_FROM_FC;

  if (isJumbo) {
    buf[3] = JUMBO_FRAME_MIN_SIZE;
    buf[4] = command & 0xff;
    buf[5] = dataLength & 0xff;
    buf[6] = (dataLength >> 8) & 0xff;
    buf.set(payload, 7);
  } else {
    buf[3] = dataLength;
    buf[4] = command & 0xff;
    buf.set(payload, 5);
  }

  buf[buf.length - 1] = xorChecksum(dataLength, command, payload);
  return buf;
}

export function encodeResponseV2(command: number, payload: Uint8Array, isError = false): Uint8Array {
  const dataLength = payload.length;
  const bufferSize = 9 + dataLength;
  const buf = new Uint8Array(bufferSize);

  buf[0] = DOLLAR;
  buf[1] = X_CHAR;
  buf[2] = isError ? DIR_ERROR : DIR_FROM_FC;
  buf[3] = 0; // flags
  buf[4] = command & 0xff;
  buf[5] = (command >> 8) & 0xff;
  buf[6] = dataLength & 0xff;
  buf[7] = (dataLength >> 8) & 0xff;
  buf.set(payload, 8);

  buf[bufferSize - 1] = crc8DvbS2(buf, 3, bufferSize - 1);
  return buf;
}
