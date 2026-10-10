import { describe, expect, it } from "vitest";

import { batteryIsMeasured, batteryReading, computeAlerts, type AlertInputs } from "@/lib/alerts";
import type { BatteryHealth, VehicleState } from "@/lib/types";

function inputs(over: Partial<AlertInputs> = {}): AlertInputs {
  return { status: null, telemetry: null, battery: null, msSinceLive: 0, video: "live", ...over };
}

function fc(remaining: number | null, voltage: number | null, armed = false): VehicleState {
  return {
    armed,
    battery: { voltage, current: null, remaining, temperature: null },
  };
}

function engine(remaining: number | null, over: Partial<BatteryHealth> = {}): BatteryHealth {
  return {
    enabled: true,
    stale: false,
    updated_at_ms: 1,
    packs: [{ id: 0, stale: false, remaining_pct: remaining, prediction: { eta_s: 240 } }],
    ...over,
  };
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

describe("battery alerts never fire on an unknown battery", () => {
  it("ignores the FC's 0% at 0.0 V (no battery monitor)", () => {
    expect(computeAlerts(inputs({ telemetry: fc(0, 0) }))).toEqual([]);
  });

  it("ignores the unknown-remaining sentinel and null", () => {
    expect(computeAlerts(inputs({ telemetry: fc(-1, 16.2) }))).toEqual([]);
    expect(computeAlerts(inputs({ telemetry: fc(null, 16.2) }))).toEqual([]);
  });

  it("ignores a stale or empty battery engine and falls back to the FC", () => {
    expect(batteryReading(engine(5, { stale: true }), fc(-1, 16))).toBeNull();
    expect(batteryReading(engine(null), null)).toBeNull();
    expect(batteryReading(engine(-1), fc(50, 16))).toEqual({
      pct: 50,
      timeToReserveS: null,
      source: "fc",
    });
  });

  it("alerts on a measured low or critical pack", () => {
    expect(computeAlerts(inputs({ telemetry: fc(18, 15.1) }))[0]).toMatchObject({
      id: "battery",
      level: "warning",
    });
    expect(computeAlerts(inputs({ battery: engine(8) }))[0]).toMatchObject({
      id: "battery",
      level: "critical",
    });
  });

  it("prefers the battery engine and carries its time to reserve", () => {
    expect(batteryReading(engine(64), fc(10, 15))).toEqual({
      pct: 64,
      timeToReserveS: 240,
      source: "engine",
    });
  });
});

describe("link alerts", () => {
  it("is critical only when lost while armed", () => {
    const armed = fc(null, null, true);
    expect(computeAlerts(inputs({ telemetry: armed, msSinceLive: 12_000 }))[0]).toMatchObject({
      id: "link-lost",
      level: "critical",
    });
    expect(computeAlerts(inputs({ telemetry: fc(null, null), msSinceLive: 12_000 }))[0]).toMatchObject({
      id: "link-stale",
      level: "warning",
    });
  });

  it("stays quiet when the vehicle was never live", () => {
    expect(computeAlerts(inputs({ msSinceLive: null }))).toEqual([]);
  });
});

describe("ordering", () => {
  it("puts critical before warning before advisory", () => {
    const alerts = computeAlerts(
      inputs({
        telemetry: { ...fc(5, 14, true), gps: { fix_type: 2, satellites: 5, eph: null, epv: null } },
        video: "frozen",
        status: {
          network: { uplink_type: "wifi", uplink_reachable: false },
        } as AlertInputs["status"],
      }),
    );
    expect(alerts.map((a) => a.level)).toEqual(["critical", "warning", "warning", "advisory"]);
  });
});
