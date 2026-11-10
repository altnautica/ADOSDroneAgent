import { describe, expect, it } from "vitest";

import { clampSplit, gridMeasures, onscreenSticksShown, SPLIT_DEFAULT, STICK_PAD_PX, widthClassFor } from "@/lib/cockpit-layout";

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

  it("keeps the split divider at 0.3–0.7, 0.5 by default (views.json split.divider)", () => {
    expect(SPLIT_DEFAULT).toBe(0.5);
    expect(clampSplit(0.05)).toBe(0.3);
    expect(clampSplit(0.95)).toBe(0.7);
    expect(clampSplit(0.55)).toBe(0.55);
    expect(clampSplit(Number.NaN)).toBe(0.5);
  });
});

describe("on-screen sticks (layout.json on_screen_sticks)", () => {
  it("show only in the spec's classes, in landscape, with no controller", () => {
    expect(onscreenSticksShown(true, false, false, "compact")).toBe(true);
    expect(onscreenSticksShown(true, false, false, "expanded")).toBe(true);
    expect(onscreenSticksShown(true, false, false, "medium")).toBe(false);
    expect(onscreenSticksShown(true, false, false, "tv")).toBe(false);
    expect(onscreenSticksShown(true, true, false, "compact")).toBe(false);
    expect(onscreenSticksShown(true, false, true, "compact")).toBe(false);
    expect(onscreenSticksShown(false, false, false, "compact")).toBe(false);
    expect(STICK_PAD_PX).toBe(120);
  });
});
