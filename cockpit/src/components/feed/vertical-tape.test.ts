// The tape's tick generator, which is the one place in the cockpit where bad
// telemetry could hang the panel outright rather than merely render wrong.
//
// The loop used to accumulate `t += step` over a value bounded only by
// `Number.isFinite`. A corrupt MAVLink float is a plain f32, so it can arrive
// finite and enormous; past roughly 1e17 the float ULP exceeds `step`, the
// accumulator stops advancing, and the loop never terminates. Nothing is
// thrown, so the error boundary cannot catch it — the kiosk just freezes with
// no diagnostic and no way in.

import { describe, expect, it } from "vitest";

import {
  isPlottableTapeValue,
  visibleTicks,
} from "@/components/feed/vertical-tape";

describe("isPlottableTapeValue", () => {
  it("accepts ordinary altitudes and speeds, including negatives", () => {
    for (const v of [0, -16.34, 120, 4500, -500]) {
      expect(isPlottableTapeValue(v)).toBe(true);
    }
  });

  it("rejects absent and non-finite values", () => {
    for (const v of [null, undefined, NaN, Infinity, -Infinity]) {
      expect(isPlottableTapeValue(v as number | null)).toBe(false);
    }
  });

  it("rejects a finite float large enough to stall the tick loop", () => {
    // The actual hang condition. These are all finite, so the old
    // `Number.isFinite` guard passed them straight through.
    for (const v of [1e17, 1e30, 3.4e38, -1e30]) {
      expect(Number.isFinite(v)).toBe(true);
      expect(isPlottableTapeValue(v)).toBe(false);
    }
  });
});

describe("visibleTicks", () => {
  it("produces the expected scale around a value", () => {
    // 40 m window, 10 m steps, centred on 100 → the ticks either side.
    expect(visibleTicks(100, 40, 10)).toEqual([80, 90, 100, 110, 120]);
  });

  it("handles negative values (below-launch altitude is real)", () => {
    expect(visibleTicks(-16, 40, 10)).toEqual([-30, -20, -10, 0]);
  });

  it("terminates and stays bounded for every rejected magnitude", () => {
    // Belt to the guard's braces: even if a caller skips
    // `isPlottableTapeValue`, the generator itself must return rather than
    // spin. If this test ever hangs, that is the bug it exists to catch.
    for (const v of [1e17, 1e30, 3.4e38]) {
      const ticks = visibleTicks(v, 40, 10);
      expect(ticks.length).toBeLessThanOrEqual(512);
    }
  });

  it("refuses a step that could never advance the scale", () => {
    expect(visibleTicks(100, 40, 0)).toEqual([]);
    expect(visibleTicks(100, 40, -5)).toEqual([]);
    expect(visibleTicks(100, 40, NaN)).toEqual([]);
  });

  it("caps the tick count rather than generating an unbounded array", () => {
    // A huge window with a tiny step would otherwise allocate millions of
    // ticks, none of which can be read on a 480-pixel-tall panel.
    expect(visibleTicks(0, 1e9, 1).length).toBe(512);
  });
});
