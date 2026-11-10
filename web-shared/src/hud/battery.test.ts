import { describe, expect, it } from "vitest";

import layout from "../cockpit-spec/layout.json";
import { BATTERY_LOW_PCT, batteryIsMeasured, batteryLowPct, batteryReading, type BatteryHealthInput, type FcBatteryInput } from "./battery";

const fc = (remaining: number | null, voltage: number | null): FcBatteryInput => ({ battery: { voltage, remaining } });

function engine(remaining: number | null, over: Partial<BatteryHealthInput> = {}): BatteryHealthInput {
  return { enabled: true, stale: false, packs: [{ id: 0, stale: false, remaining_pct: remaining, prediction: { eta_s: 240 } }], ...over };
}

describe("batteryIsMeasured", () => {
  it("rejects a board with nothing attached and the no-monitor sentinel", () => {
    expect(batteryIsMeasured(0)).toBe(false);
    expect(batteryIsMeasured(65.535)).toBe(false);
  });

  it("accepts real packs, including a 65.5 V stack", () => {
    expect(batteryIsMeasured(65.5)).toBe(true);
    expect(batteryIsMeasured(22.2)).toBe(true);
  });

  it("treats absent and non-finite readings as unmeasured", () => {
    expect(batteryIsMeasured(null)).toBe(false);
    expect(batteryIsMeasured(undefined)).toBe(false);
    expect(batteryIsMeasured(Number.NaN)).toBe(false);
  });
});

describe("batteryReading", () => {
  it("is unknown for the FC's 0% at 0.0 V, the -1 sentinel and null", () => {
    expect(batteryReading(null, fc(0, 0))).toBeNull();
    expect(batteryReading(null, fc(-1, 16.2))).toBeNull();
    expect(batteryReading(null, fc(null, 16.2))).toBeNull();
  });

  it("ignores a stale or empty battery engine and falls back to the FC", () => {
    expect(batteryReading(engine(5, { stale: true }), fc(-1, 16))).toBeNull();
    expect(batteryReading(engine(null), null)).toBeNull();
    expect(batteryReading(engine(-1), fc(50, 16))).toEqual({ pct: 50, timeToReserveS: null, source: "fc" });
  });

  it("prefers the battery engine and carries its time to reserve", () => {
    expect(batteryReading(engine(64), fc(10, 15))).toEqual({ pct: 64, timeToReserveS: 240, source: "engine" });
  });
});

describe("battery bands", () => {
  it("takes the low fallback from the spec and the engine's reserve above it", () => {
    expect(BATTERY_LOW_PCT).toBe(layout.battery.low_fallback_pct);
    expect(batteryLowPct(null)).toBe(BATTERY_LOW_PCT);
    expect(batteryLowPct({ thresholds: { reserve_percent: 30 } })).toBe(30);
    expect(batteryLowPct({ thresholds: { reserve_percent: 5 } })).toBe(BATTERY_LOW_PCT);
  });
});
