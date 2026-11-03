import { describe, expect, it } from "vitest";

import {
  THRESHOLD_FIELDS,
  fmtEta,
  fmtRuleValue,
  liveView,
  thresholdValid,
  type BatteryHealthDetail,
} from "@/lib/settings/battery";
import { stepLeafChange } from "@/lib/settings/guarded-leaf";

const health = (over: Partial<BatteryHealthDetail> = {}): BatteryHealthDetail => ({
  enabled: true,
  stale: false,
  updated_at_ms: 1,
  packs: [],
  ...over,
});

describe("battery live view", () => {
  it("waits for the first poll, then reports a failure without a snapshot", () => {
    expect(liveView(null, false)).toBe("loading");
    expect(liveView(null, true)).toBe("error");
  });

  it("separates monitoring off, no packs and packs", () => {
    expect(liveView(health({ enabled: false }), true)).toBe("off");
    expect(liveView(health(), true)).toBe("empty");
    const pack = { id: 0 } as BatteryHealthDetail["packs"][number];
    expect(liveView(health({ packs: [pack] }), true)).toBe("packs");
  });
});

describe("battery formatting", () => {
  it("formats each rule in the unit the engine reports", () => {
    expect(fmtRuleValue("cell_low", 3.456)).toBe("3.46 V");
    expect(fmtRuleValue("cell_divergence", 41.6)).toBe("42 mV");
    expect(fmtRuleValue("voltage_drop", 1.234)).toBe("1.23 V/s");
    expect(fmtRuleValue("temp_spike", 2.25)).toBe("2.3 °C/s");
    expect(fmtRuleValue("predictive_low", 89.6)).toBe("90 s");
  });

  it("formats time to reserve", () => {
    expect(fmtEta(null)).not.toMatch(/\d/);
    expect(fmtEta(42)).toBe("42s");
    expect(fmtEta(125)).toBe("2m 05s");
  });
});

describe("battery thresholds", () => {
  it("covers every engine threshold with the config model bounds", () => {
    expect(THRESHOLD_FIELDS.map((f) => f.key)).toEqual([
      "low_cell_mv",
      "critical_cell_mv",
      "cell_divergence_mv",
      "voltage_drop_mv_per_s",
      "temp_spike_dc_per_s",
      "predictive_window_s",
      "reserve_percent",
    ]);
    const reserve = THRESHOLD_FIELDS.find((f) => f.key === "reserve_percent")!;
    expect(thresholdValid(reserve, 25)).toBe(true);
    expect(thresholdValid(reserve, 4)).toBe(false);
    expect(thresholdValid(reserve, 51)).toBe(false);
    expect(thresholdValid(reserve, 25.5)).toBe(false);
  });

  it("writes thresholds without a guard", () => {
    expect(stepLeafChange({ key: "battery.reserve_percent", value: 30 }, false).kind).toBe("write");
  });
});
