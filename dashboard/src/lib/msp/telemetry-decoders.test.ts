import { describe, expect, it } from "vitest";

import { decodeInavStatus, decodeRawGps, gpsFixLabel } from "./telemetry-decoders";

/** An MSP2_INAV_STATUS reply laid out the way iNav writes it: U16 cycleTime,
 *  U16 i2cErrors, U16 sensors, U16 system load, U8 profiles, U32 armingFlags,
 *  an 8-byte box-mode bitmask, U8 mixer profile. */
function inavStatus(opts: { load: number; armingFlags: number; boxModes: number }): Uint8Array {
  const buf = new Uint8Array(22);
  const dv = new DataView(buf.buffer);
  dv.setUint16(0, 1000, true);
  dv.setUint16(2, 0, true);
  dv.setUint16(4, 0b1011, true);
  dv.setUint16(6, opts.load, true);
  dv.setUint8(8, 0x10);
  dv.setUint32(9, opts.armingFlags, true);
  dv.setUint32(13, opts.boxModes, true);
  dv.setUint32(17, 0, true);
  dv.setUint8(21, 0);
  return buf;
}

describe("decodeInavStatus", () => {
  it("reads armed and the system load from the offsets iNav writes them at", () => {
    // ARMED is bit 2 of armingFlags. The box-mode bitmask right after it is
    // all ones, so a decoder reading the wrong offset reports garbage.
    const armed = decodeInavStatus(
      inavStatus({ load: 37, armingFlags: 1 << 2, boxModes: 0xffffffff }),
    );
    expect(armed?.armed).toBe(true);
    expect(armed?.cpuLoad).toBe(37);

    const disarmed = decodeInavStatus(
      inavStatus({ load: 12, armingFlags: 1 << 3, boxModes: 0xffffffff }),
    );
    expect(disarmed?.armed).toBe(false);
    expect(disarmed?.cpuLoad).toBe(12);
  });
});

/** An MSP_RAW_GPS reply with the trailing DOP field. */
function rawGps(fix: number, dop: number): Uint8Array {
  const buf = new Uint8Array(18);
  const dv = new DataView(buf.buffer);
  dv.setUint8(0, fix);
  dv.setUint8(1, 11);
  dv.setUint16(16, dop, true);
  return buf;
}

describe("MSP_RAW_GPS decoding", () => {
  it("reads Betaflight's fix byte as a has-fix flag and its DOP as PDOP", () => {
    const g = decodeRawGps(rawGps(1, 145), "betaflight");
    expect(gpsFixLabel(g!.fixType, "betaflight")).toBe("fix");
    expect(g?.dopKind).toBe("pdop");
    expect(g?.dop).toBeCloseTo(1.45);
  });

  it("reads iNav's fix byte as a fix type and its DOP as HDOP", () => {
    const g = decodeRawGps(rawGps(2, 90), "inav");
    expect(gpsFixLabel(g!.fixType, "inav")).toBe("3D");
    expect(gpsFixLabel(1, "inav")).toBe("2D");
    expect(g?.dopKind).toBe("hdop");
  });
});
