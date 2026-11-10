// Where the flight-screen tips may sit: each spec anchor and zone as the
// element that carries it on the Feed, and the rects a tip must stay clear of.

import tipsJson from "@/shared/cockpit-spec/tips.json";
import type { TipRect } from "@/shared/hud/tips";
import { FLIGHT_OVERLAYS_SELECTOR } from "@/stores/tips-store";

/** Each spec anchor and zone, as the element that carries it on the Feed. */
export const TIP_SELECTORS: Readonly<Record<string, string>> = {
  status_pill: "[data-zone=status_pill]",
  flight_state_chip: "[data-testid=flight-state-chip]",
  layer_button: "[data-zone=layer_button]",
  stop: "[data-zone=stop]",
  skill_picker_chip: "[data-testid=skill-picker-chip]",
  bottom_left_card: "[data-zone=bottom_left_card]",
  gimbal_pad: "[data-zone=gimbal_pad]",
  left_column: "[data-zone=left_column]",
  top_bar: "[data-zone=top_bar]",
  alert_stack: "[data-zone=alert_stack]",
};

const NEVER_COVER: readonly string[] = tipsJson.rules.never_cover_zones;

/** The element's rect when it is laid out and on screen, else null. */
export function visibleRect(selector: string): TipRect | null {
  const el = document.querySelector(selector);
  return el ? elementRect(el) : null;
}

/** The element's rect when it is laid out and on screen, else null. */
export function elementRect(el: Element): TipRect | null {
  const r = el.getBoundingClientRect();
  if (r.width <= 0 || r.height <= 0) return null;
  if (r.right <= 0 || r.bottom <= 0 || r.left >= window.innerWidth || r.top >= window.innerHeight) return null;
  if (getComputedStyle(el).visibility === "hidden") return null;
  return { x: r.left, y: r.top, w: r.width, h: r.height };
}

/** The rects a tip must stay clear of: every `never_cover_zones` entry (the
 *  top bar among them, so a tip never hides the bar's own readouts) and the
 *  flow overlays (launch step and Abort, countdowns, sticks banner).
 *  `rectOf` measures an element; exported for tests. */
export function tipAvoidRects(rectOf: (el: Element) => TipRect | null = elementRect): TipRect[] {
  const selectors = [...NEVER_COVER.flatMap((z) => (TIP_SELECTORS[z] ? [TIP_SELECTORS[z]] : [])), FLIGHT_OVERLAYS_SELECTOR];
  return selectors.flatMap((sel) => {
    const el = document.querySelector(sel);
    const r = el ? rectOf(el) : null;
    return r ? [r] : [];
  });
}
