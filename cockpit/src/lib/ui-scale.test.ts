// @vitest-environment happy-dom
// The root font-size formula: bounded at both ends, linear in the short side
// between them, the UI-scale knob acting at every size, and the stylesheet
// declaring the same formula. The flight popovers draw at the chrome scale.

import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

import { popoverStyle } from "./chrome-tokens";
import { chromeScale, REFERENCE_ROOT_PX, ROOT_FONT, rootFontPx } from "./ui-scale";

describe("the root font-size", () => {
  it("holds the 16 px floor on small panels and phones", () => {
    expect(rootFontPx(800, 480)).toBe(16);
    expect(rootFontPx(1024, 600)).toBe(16);
    expect(rootFontPx(390, 844)).toBe(16);
  });

  it("grows with the short side between the bounds", () => {
    expect(rootFontPx(1280, 800)).toBe(18);
    expect(rootFontPx(1366, 768)).toBeCloseTo(17.68);
    expect(rootFontPx(1440, 900)).toBe(19);
    expect(rootFontPx(900, 1000)).toBe(19);
    expect(rootFontPx(1920, 1080)).toBeCloseTo(20.8);
  });

  it("caps at 22 px on large monitors", () => {
    expect(rootFontPx(2560, 1440)).toBe(22);
    expect(rootFontPx(3840, 2160)).toBe(22);
  });

  it("multiplies the bounded value by the UI scale, so the knob acts at both bounds", () => {
    expect(rootFontPx(800, 480, 1.2)).toBeCloseTo(19.2);
    expect(rootFontPx(3840, 2160, 1.2)).toBeCloseTo(26.4);
    expect(rootFontPx(3840, 2160, 0.8)).toBeCloseTo(17.6);
  });

  it("gives the flight chrome root / 16", () => {
    expect(chromeScale(800, 480)).toBe(1);
    expect(chromeScale(1920, 1080)).toBe(1.3);
    expect(chromeScale(2560, 1440)).toBe(22 / REFERENCE_ROOT_PX);
    expect(chromeScale(1280, 800, 1.25)).toBe(1.406);
  });

  it("is the formula the stylesheet declares", () => {
    const css = readFileSync(path.resolve(__dirname, "../styles/globals.css"), "utf8");
    const m = /html\s*\{[^}]*font-size:\s*calc\(clamp\(([\d.]+)px,\s*([\d.]+)px \+ ([\d.]+)vmin,\s*([\d.]+)px\) \* var\(--ui-scale\)\);/.exec(css);
    expect(m).not.toBeNull();
    expect(m!.slice(1).map(Number)).toEqual([ROOT_FONT.minPx, ROOT_FONT.basePx, ROOT_FONT.vminFactor, ROOT_FONT.maxPx]);
  });
});

describe("a flight popover at the chrome scale", () => {
  const anchor = (left: number, right: number, bottom: number) => ({ left, right, bottom, top: bottom - 40, width: right - left, height: 40, x: left, y: bottom - 40, toJSON: () => ({}) }) as DOMRect;

  it("zooms by the scale and gives its offsets unscaled, so it lands under its anchor", () => {
    Object.assign(window, { innerWidth: 1920, innerHeight: 1080 });
    const { panel, maxHeight } = popoverStyle(anchor(300, 500, 60), 440, 1.3);
    expect(panel.zoom).toBe(1.3);
    expect(Number(panel.left) * 1.3).toBeCloseTo(300);
    expect(Number(panel.top) * 1.3).toBeCloseTo(68);
    expect(Number(panel.width)).toBeCloseTo(440);
    expect(maxHeight * 1.3).toBeCloseTo(1080 - 68 - 8);
  });

  it("stays on screen, never wider than the window less its margins", () => {
    Object.assign(window, { innerWidth: 400, innerHeight: 800 });
    const { panel } = popoverStyle(anchor(300, 380, 60), 440, 1);
    expect(panel.zoom).toBeUndefined();
    expect(panel.left).toBe(8);
    expect(panel.width).toBe(384);
    const right = popoverStyle(anchor(200, 260, 60), 220, 1, "right").panel;
    expect(Number(right.left) + Number(right.width)).toBe(260);
  });
});
