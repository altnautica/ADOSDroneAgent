import { describe, expect, it } from "vitest";

import {
  captureStateTone,
  exposureDirty,
  exposureNotApplied,
  exposureRequest,
  exposureSeedKey,
  toDraft,
} from "@/lib/settings/capture";
import type { CaptureExposure } from "@/shared/api/capture";

const reported: CaptureExposure = { locked: true, exposure_us: 2000, gain: 1.5, mode: "walking", applied: true };

describe("exposure draft", () => {
  it("seeds from the agent's values, blank where none is reported", () => {
    expect(toDraft(reported)).toEqual({ locked: true, exposureUs: "2000", gain: "1.5", mode: "walking" });
    expect(toDraft({ locked: false, exposure_us: null, gain: null, mode: null })).toEqual({
      locked: false,
      exposureUs: "",
      gain: "",
      mode: null,
    });
  });

  it("builds a request only from positive numbers and a mode", () => {
    expect(exposureRequest(toDraft(reported))).toEqual({ locked: true, exposure_us: 2000, gain: 1.5, mode: "walking" });
    const base = toDraft(reported);
    expect(exposureRequest({ ...base, exposureUs: "" })).toBeNull();
    expect(exposureRequest({ ...base, exposureUs: "0" })).toBeNull();
    expect(exposureRequest({ ...base, gain: "-1" })).toBeNull();
    expect(exposureRequest({ ...base, gain: "abc" })).toBeNull();
    expect(exposureRequest({ ...base, mode: null })).toBeNull();
    // A half-typed decimal is still a number.
    expect(exposureRequest({ ...base, gain: "2." })?.gain).toBe(2);
  });

  it("is dirty only when an edited field differs", () => {
    const base = toDraft(reported);
    expect(exposureDirty(base, reported)).toBe(false);
    expect(exposureDirty({ ...base, gain: " 1.5 " }, reported)).toBe(false);
    expect(exposureDirty({ ...base, locked: false }, reported)).toBe(true);
    expect(exposureDirty({ ...base, mode: "slow" }, reported)).toBe(true);
  });

  it("re-seeds on a changed value, not on the applied flag alone", () => {
    expect(exposureSeedKey(reported)).toBe(exposureSeedKey({ ...reported, applied: false }));
    expect(exposureSeedKey(reported)).not.toBe(exposureSeedKey({ ...reported, gain: 2 }));
    expect(exposureSeedKey(null)).toBe("");
  });

  it("flags a requested lock the camera has not taken", () => {
    expect(exposureNotApplied({ ...reported, applied: false })).toBe(true);
    expect(exposureNotApplied(reported)).toBe(false);
    expect(exposureNotApplied({ ...reported, locked: false, applied: false })).toBe(false);
  });
});

describe("captureStateTone", () => {
  it("colours the budget state", () => {
    expect(captureStateTone("ok")).toBe("ok");
    expect(captureStateTone("near_full")).toBe("warn");
    expect(captureStateTone("full")).toBe("err");
  });
});
