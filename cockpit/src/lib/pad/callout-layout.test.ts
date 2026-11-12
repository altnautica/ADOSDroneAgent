import { describe, expect, it } from "vitest";

import { DRAWINGS, type Hotspot } from "./art";
import { CALLOUT_MAX_CHARS, CALLOUT_PITCH, fitLabel, hitRadii, layoutCallouts, pathBox } from "./callout-layout";

const centre = (h: Hotspot): [number, number] =>
  h.shape === "circle" ? [h.geom.cx, h.geom.cy] : h.shape === "rect" ? [h.geom.x + h.geom.w / 2, h.geom.y + h.geom.h / 2] : pathBox(h.geom.d).centre;

describe("controller drawing layout", () => {
  for (const [kind, d] of Object.entries(DRAWINGS)) {
    const [x, y, w, h] = d.art.viewBox;
    const items = Object.entries(d.hotspots).map(([control, hs]) => ({
      control,
      label: "Swap video / map and a long action name",
      labelSide: hs!.label_side,
      anchor: hs!.label_anchor,
    }));

    it(`${kind}: callouts never overlap and stay inside the drawing`, () => {
      const out = layoutCallouts(items, { x, y, w, h });
      expect(out).toHaveLength(items.length);
      for (const side of ["left", "right"] as const) {
        const ys = out.filter((c) => c.side === side).map((c) => c.y).sort((a, b) => a - b);
        for (let i = 1; i < ys.length; i++) expect(ys[i] - ys[i - 1]).toBeGreaterThanOrEqual(CALLOUT_PITCH - 1e-9);
        for (const v of ys) {
          expect(v).toBeGreaterThanOrEqual(y);
          expect(v).toBeLessThanOrEqual(y + h);
        }
      }
    });

    it(`${kind}: every control has a hit area of at least the wanted size or half its neighbour gap`, () => {
      const centres = Object.fromEntries(Object.entries(d.hotspots).map(([c, hs]) => [c, centre(hs!)]));
      const radii = hitRadii(centres, {}, 24);
      for (const [c, r] of Object.entries(radii)) {
        const [cx, cy] = centres[c];
        const gaps = Object.entries(centres).filter(([o]) => o !== c).map(([, [ox, oy]]) => Math.hypot(ox - cx, oy - cy)).filter((g) => g > 0);
        expect(r, c).toBeGreaterThanOrEqual(Math.min(24, Math.min(...gaps) / 2) - 1e-9);
      }
    });
  }

  it("labels are cut to the column with an ellipsis", () => {
    expect(fitLabel("Select / GO / confirm the active skill").length).toBeLessThanOrEqual(CALLOUT_MAX_CHARS);
    expect(fitLabel("Select / GO / confirm the active skill").endsWith("…")).toBe(true);
    expect(fitLabel("Pause (brake)")).toBe("Pause (brake)");
  });

  it("a D-pad path hit area centres on the arrow, not its first corner", () => {
    expect(pathBox("M 223 240 H 257 V 278 L 240 294 L 223 278 Z").centre).toEqual([240, 267]);
  });
});
