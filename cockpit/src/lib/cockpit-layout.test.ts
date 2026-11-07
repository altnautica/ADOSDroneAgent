import { describe, expect, it } from "vitest";

import { clampSplit, gridMeasures, SPLIT_DEFAULT, widthClassFor } from "@/lib/cockpit-layout";

describe("width classes (cockpit spec layout.json)", () => {
  it("splits at 600, 840 and 1600 px", () => {
    expect(widthClassFor(599, false)).toBe("compact");
    expect(widthClassFor(600, false)).toBe("medium");
    expect(widthClassFor(839, false)).toBe("medium");
    expect(widthClassFor(840, false)).toBe("expanded");
    expect(widthClassFor(1599, false)).toBe("expanded");
    expect(widthClassFor(1600, false)).toBe("tv");
    expect(widthClassFor(3840, false)).toBe("tv");
  });

  it("treats the kiosk as a tv at any width", () => {
    expect(widthClassFor(800, true)).toBe("tv");
  });
});

describe("grid measures", () => {
  it("shrinks the columns to 48 and shows the card as a 96 px chip on compact", () => {
    const m = gridMeasures("compact", false);
    expect(m.colPx).toBe(48);
    expect(m.card).toEqual({ kind: "chip", sizePx: 96 });
    expect(m.distanceOnly).toBe(true);
    expect(m.defaultLevel).toBe("route");
  });

  it("keeps the shutter at 56 even where columns shrink", () => {
    expect(gridMeasures("compact", false).shutterPx).toBe(56);
  });

  it("scales a tv by 1.33 with focus rings always on and a 30 fps overlay cap", () => {
    const m = gridMeasures("tv", false);
    expect(m.scale).toBe(1.33);
    expect(m.focusRingsAlways).toBe(true);
    expect(m.overlayFpsCap).toBe(30);
    expect(gridMeasures("expanded", false).focusRingsAlways).toBe(false);
  });

  it("grows touch targets in glove mode", () => {
    expect(gridMeasures("medium", true).colPx).toBe(70);
  });

  it("keeps the split between 0.2 and 0.8", () => {
    expect(clampSplit(0.05)).toBe(0.2);
    expect(clampSplit(0.95)).toBe(0.8);
    expect(clampSplit(0.5)).toBe(0.5);
    expect(clampSplit(Number.NaN)).toBe(SPLIT_DEFAULT);
  });
});
