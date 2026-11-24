// The cockpit's one size source: the root font-size. Everything scales off
// it, so a panel of any size shows one consistent type scale.
//
// - The root tracks the viewport's shorter side, bounded: 16 px up to a
//   600 px short side (the 800×480 and 1024×600 panels), growing 1 px per
//   100 px of short side to 22 px at 1200 px and above. A large window shows
//   more content, never a giant copy of the small layout. The operator's
//   UI-scale knob multiplies the bounded value, so it acts at every size.
// - Framed screens and sheets size in rem against it.
// - The flight screen's chrome is authored in CSS px against a 16 px root
//   (the cockpit spec tokens) and drawn at the chrome scale, root / 16, as a
//   CSS zoom on each zone and popover (or px × scale where a surface sits
//   outside a zone). One Feed px is therefore 1/16 rem.
// - Glove mode grows touch targets only (`--touch-scale`), never text.
//
// `styles/globals.css` declares the same formula for `html { font-size }`;
// a test keeps the two in step.

import { useSyncExternalStore } from "react";

import { useSettingsStore } from "@/stores/settings-store";

/** The root font-size bounds and slope: clamp(MIN, BASE + VMIN·1vmin, MAX). */
export const ROOT_FONT = { minPx: 16, basePx: 10, vminFactor: 1, maxPx: 22 } as const;

/** The reference root the flight-screen tokens are authored against. */
export const REFERENCE_ROOT_PX = 16;

/** The root font-size in CSS px for a viewport `width`×`height` and the
 *  operator's UI scale. */
export function rootFontPx(width: number, height: number, uiScale = 1): number {
  const vmin = Math.min(width, height) / 100;
  const fluid = ROOT_FONT.basePx + ROOT_FONT.vminFactor * vmin;
  return Math.min(ROOT_FONT.maxPx, Math.max(ROOT_FONT.minPx, fluid)) * uiScale;
}

/** The flight-screen chrome scale: the root against the 16 px reference. */
export function chromeScale(width: number, height: number, uiScale = 1): number {
  return Math.round((rootFontPx(width, height, uiScale) / REFERENCE_ROOT_PX) * 1000) / 1000;
}

function subscribe(onChange: () => void): () => void {
  window.addEventListener("resize", onChange);
  return () => window.removeEventListener("resize", onChange);
}

const readWidth = () => window.innerWidth;
const readHeight = () => window.innerHeight;

/** The live chrome scale: the window size and the UI-scale knob. */
export function useChromeScale(): number {
  const uiScale = useSettingsStore((s) => s.uiScale);
  const w = useSyncExternalStore(subscribe, readWidth, () => 800);
  const h = useSyncExternalStore(subscribe, readHeight, () => 480);
  return chromeScale(w, h, uiScale);
}
