import { describe, expect, it } from "vitest";

import { txAdapterMissing, txPowerBlock, txPowerBounds } from "@/lib/transmit-model";
import type { WfbStatus } from "@/shared/api/types";

const wfb = (over: Partial<WfbStatus>): WfbStatus => ({ state: "active", interface: "wlan1", tx_power_max_dbm: 15, ...over }) as WfbStatus;

describe("TX power bounds", () => {
  it("bounds the write to the region ceiling the radio reports", () => {
    expect(txPowerBounds(wfb({}))).toEqual({ min: 1, max: 15 });
    expect(txPowerBounds(wfb({ tx_power_max_dbm: 20.6 }))).toEqual({ min: 1, max: 20 });
  });

  it("offers no write without a usable ceiling", () => {
    expect(txPowerBounds(wfb({ tx_power_max_dbm: 0 }))).toBeNull();
    expect(txPowerBounds(null)).toBeNull();
  });

  it("reads a missing transmitter", () => {
    expect(txAdapterMissing(wfb({ state: "disabled" }))).toBe(true);
    expect(txAdapterMissing(wfb({ state: "error", interface: "" }))).toBe(true);
    expect(txAdapterMissing(wfb({ state: "error" }))).toBe(false);
  });
});

describe("TX power confirm gate", () => {
  const bounds = { min: 1, max: 15 };
  it("allows an in-range change while disarmed", () => {
    expect(txPowerBlock(10, 12, bounds, null)).toBeNull();
  });

  it("refuses while armed or with the arm state unknown, before range checks", () => {
    expect(txPowerBlock(10, 12, bounds, "armed")).toBe("armed");
    expect(txPowerBlock(99, 12, bounds, "armedUnknown")).toBe("armedUnknown");
  });

  it("refuses outside the region clamp, a fraction, the current value, and no radio", () => {
    expect(txPowerBlock(16, 12, bounds, null)).toBe("outOfRange");
    expect(txPowerBlock(0, 12, bounds, null)).toBe("outOfRange");
    expect(txPowerBlock(10.5, 12, bounds, null)).toBe("outOfRange");
    expect(txPowerBlock(12, 12, bounds, null)).toBe("unchanged");
    expect(txPowerBlock(10, 12, null, null)).toBe("noRadio");
  });
});
