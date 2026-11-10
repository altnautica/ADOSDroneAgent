// The target layer's pointer mapping under the digital zoom, and when it
// raises "Targets unavailable".

import { describe, expect, it } from "vitest";

import { layoutPoint, showTargetsUnavailable } from "@/components/feed/target-pointer";
import { computeRenderedRect } from "@/lib/overlay-geometry";
import { dragFrameBox, dragScreenRect } from "@/shared/hud/target-interaction";

describe("layoutPoint", () => {
  // A 400 × 300 layer scaled ×2 about its centre: its on-screen rect is
  // 800 × 600, starting 200 px left of and 150 px above the unscaled box.
  const zoomed = {
    clientWidth: 400,
    clientHeight: 300,
    getBoundingClientRect: () => ({ left: -200, top: -150, width: 800, height: 600 }),
  };

  it("maps the screen through the zoom into layout pixels", () => {
    expect(layoutPoint(200, 150, zoomed)).toEqual({ x: 200, y: 150 });
    expect(layoutPoint(-200, -150, zoomed)).toEqual({ x: 0, y: 0 });
  });

  it("at zoom 2 a drag at the screen centre locks the centre box", () => {
    const rendered = computeRenderedRect(400, 300, 640, 480);
    const a = layoutPoint(180, 135, zoomed);
    const b = layoutPoint(220, 165, zoomed);
    const box = dragFrameBox(dragScreenRect(a, b), rendered, 640, 480);
    expect(box.x + box.width / 2).toBeCloseTo(320, 0);
    expect(box.y + box.height / 2).toBeCloseTo(240, 0);
  });
});

describe("showTargetsUnavailable", () => {
  it("never shows together with an idle detector", () => {
    expect(showTargetsUnavailable("hidden", "idle", true)).toBe(false);
    expect(showTargetsUnavailable("hidden", "unavailable", true)).toBe(false);
  });

  it("shows while the detector is detecting and the batch is hidden", () => {
    expect(showTargetsUnavailable("hidden", "detecting", false)).toBe(true);
    expect(showTargetsUnavailable("fresh", "detecting", true)).toBe(false);
  });

  it("with the detector unknown, shows only once a batch was picked", () => {
    expect(showTargetsUnavailable("hidden", null, true)).toBe(true);
    expect(showTargetsUnavailable("hidden", null, false)).toBe(false);
  });
});
