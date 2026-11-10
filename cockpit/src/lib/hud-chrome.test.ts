import { describe, expect, it } from "vitest";

import { CHROME_TOKENS } from "@/lib/chrome-tokens";
import { ALT_REF_BUTTON_H, HUD_UNIT_PX, altRefButtonPos, altTitleRect, hudTapes } from "@/lib/hud-chrome";

const overlaps = (a: { x: number; y: number; w: number; h: number }, b: { x: number; y: number; w: number; h: number }) =>
  a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;

describe("instruments chrome", () => {
  it("sizes the instruments from the chrome tokens, not the root font", () => {
    // A value readout (0.8 units) is the chrome value size.
    expect(HUD_UNIT_PX * 0.8).toBe(CHROME_TOKENS.type.value_px);
  });

  it("keeps the altitude-reference toggle clear of the ALT tape title at every size", () => {
    const sizes: Array<[number, number, number]> = [
      [560, 300, 1],
      [760, 420, 1],
      [1000, 560, 1],
      [1280, 800, 1],
      [1600, 900, 1.33],
      [1920, 1080, 1.33],
    ];
    for (const [w, h, scale] of sizes) {
      const u = HUD_UNIT_PX * scale;
      const t = hudTapes(w, h, u);
      const p = altRefButtonPos(w, h, u);
      const button = { x: p.x, y: p.y, w: 48 * scale, h: ALT_REF_BUTTON_H };
      expect(overlaps(button, altTitleRect(t, u)), `${w}x${h}`).toBe(false);
      expect(p.y + ALT_REF_BUTTON_H).toBeLessThanOrEqual(h);
      expect(p.x + button.w).toBeLessThanOrEqual(w);
    }
  });
});
