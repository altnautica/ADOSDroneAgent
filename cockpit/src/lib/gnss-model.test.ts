import { describe, expect, it } from "vitest";

import { canTurnBaseOff, surveyFraction, surveyRequest } from "@/lib/gnss-model";
import type { GnssBase } from "@/shared/api/sensors";

const base = (over: Partial<GnssBase>): GnssBase => ({
  mode: "survey",
  state: "surveying",
  elapsed_s: 60,
  min_dur_s: 300,
  accuracy_m: 3,
  target_accuracy_m: 2,
  fixed: null,
  rtcm_out: { msgs_per_s: 0, bytes_total: 0, targets: 0 },
  ...over,
});

describe("survey request", () => {
  it("builds a survey from positive entries", () => {
    expect(surveyRequest("300", "2.0")).toEqual({ mode: "survey", min_dur_s: 300, accuracy_m: 2 });
  });

  it("refuses empty, zero, negative, fractional-duration and non-numeric entries", () => {
    expect(surveyRequest("", "2")).toBeNull();
    expect(surveyRequest("0", "2")).toBeNull();
    expect(surveyRequest("300", "-1")).toBeNull();
    expect(surveyRequest("30.5", "2")).toBeNull();
    expect(surveyRequest("abc", "2")).toBeNull();
  });
});

describe("base-off gate", () => {
  it("offers the guarded stop only when the base runs and nothing is in flight", () => {
    expect(canTurnBaseOff(base({}), false)).toBe(true);
    expect(canTurnBaseOff(base({ mode: "off", state: "off" }), false)).toBe(false);
    expect(canTurnBaseOff(base({}), true)).toBe(false);
    expect(canTurnBaseOff(null, false)).toBe(false);
  });

  it("reports survey progress only while surveying", () => {
    expect(surveyFraction(base({}))).toBeCloseTo(0.2);
    expect(surveyFraction(base({ elapsed_s: 900 }))).toBe(1);
    expect(surveyFraction(base({ state: "fixed" }))).toBeNull();
  });
});
