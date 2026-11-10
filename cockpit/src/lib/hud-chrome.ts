// The instruments' size unit and the tape geometry the canvas draws and the
// altitude-reference toggle is placed by, so the toggle sits clear of the
// ALT tape's title at every size. The unit is CSS px times the class scale
// from the chrome tokens (a value readout is `value_px`), never the root
// font size, so the HUD does not grow with the viewport.

import { CHROME_TOKENS } from "@/lib/chrome-tokens";

/** Value readouts draw at 0.8 units. */
export const HUD_UNIT_PX = CHROME_TOKENS.type.value_px / 0.8;
/** The toggle's height (the HUD's smallest pointer target). */
export const ALT_REF_BUTTON_H = 40;

export interface HudTapes {
  /** The attitude window's half-height. */
  R: number;
  tapeH: number;
  tapeTop: number;
  speedX: number;
  altX: number;
}

export function hudTapes(w: number, h: number, u: number): HudTapes {
  const R = Math.max(40, Math.min(w * 0.26, h * 0.4));
  const tapeH = R * 1.8;
  return { R, tapeH, tapeTop: h / 2 - tapeH / 2, speedX: u * 3.4, altX: w - u * 3.4 };
}

/** The ALT tape's title box (left-aligned at the tape, centred `0.5u` above
 *  it, 7 characters at 0.6 units). */
export function altTitleRect(t: HudTapes, u: number): { x: number; y: number; w: number; h: number } {
  return { x: t.altX, y: t.tapeTop - u * 0.8, w: 7 * 0.6 * u * 0.62, h: u * 0.6 };
}

/** The altitude-reference toggle's top-left: under the ALT tape, left-aligned
 *  with it (the V/S readout there is right-aligned left of the tape), kept
 *  inside the HUD. */
export function altRefButtonPos(w: number, h: number, u: number): { x: number; y: number } {
  const t = hudTapes(w, h, u);
  return { x: t.altX, y: Math.max(0, Math.min(t.tapeTop + t.tapeH + u * 0.3, h - ALT_REF_BUTTON_H)) };
}
