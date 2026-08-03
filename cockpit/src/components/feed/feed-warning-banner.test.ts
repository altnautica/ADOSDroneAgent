import { describe, expect, it } from "vitest";

import { batteryIsMeasured } from "@/components/feed/feed-warning-banner";

// A flight controller with no battery monitor reports 0% at 0.0V, and the
// warning banner used to accept that as an empty pack — so every bench session
// without a battery attached raised a red "Battery low - 0% remaining" over the
// live video. A critical alarm that fires every session is one the operator
// learns to dismiss, and it gets dismissed just as fast on the flight where it
// is true.
describe("batteryIsMeasured", () => {
  it("rejects a board with nothing attached", () => {
    // The reading that started this: 0% at 0.0V is absent telemetry, not an
    // empty pack. A real pack at 0% still has voltage.
    expect(batteryIsMeasured(0)).toBe(false);
  });

  it("rejects the no-battery-monitor sentinel", () => {
    // MAVLink SYS_STATUS reports 65535 mV when the FC has no monitor; the agent
    // divides by 1000 before this sees it.
    expect(batteryIsMeasured(65.535)).toBe(false);
  });

  it("accepts a genuine high-voltage pack", () => {
    // 65.5 V is 65500 mV, NOT the 65535 sentinel. Rejecting by magnitude rather
    // than by the exact value would hide a real stack.
    expect(batteryIsMeasured(65.5)).toBe(true);
  });

  it("accepts an ordinary pack", () => {
    expect(batteryIsMeasured(22.2)).toBe(true);
    expect(batteryIsMeasured(3.7)).toBe(true);
  });

  it("treats absent and non-finite readings as unmeasured", () => {
    expect(batteryIsMeasured(null)).toBe(false);
    expect(batteryIsMeasured(undefined)).toBe(false);
    expect(batteryIsMeasured(Number.NaN)).toBe(false);
    expect(batteryIsMeasured(Number.POSITIVE_INFINITY)).toBe(false);
  });
});
