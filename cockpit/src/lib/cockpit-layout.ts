// Width classes and named cockpit layout measures, so each clearance is
// defined once. The breakpoints are the cockpit spec's (`layout.json`), the
// same on every surface: compact < 600, medium 600–839, expanded 840–1599,
// tv ≥ 1600 or the kiosk. The agent cockpit measures the viewport.

import { LAYOUT_CLASSES, ON_SCREEN_STICKS, TOUCH_TARGET, WIDTH_CLASSES, type LayoutClassSpec } from "@/lib/cockpit-spec";
import layoutJson from "@/shared/cockpit-spec/layout.json";
import viewsJson from "@/shared/cockpit-spec/views.json";
import { specText } from "@/shared/hud/spec-text";

/** The window width class. `tv` is a large panel or the HDMI kiosk. */
export type WidthClass = "compact" | "medium" | "expanded" | "tv";

/** The width class for a viewport `widthPx` wide; the kiosk is always `tv`. */
export function widthClassFor(widthPx: number, kiosk: boolean): WidthClass {
  if (kiosk) return "tv";
  const w = Math.floor(widthPx);
  return WIDTH_CLASSES.find((c) => w >= c.min && (c.max === null || w <= c.max))?.id ?? "compact";
}

/** Space kept free above the bottom edge for the bottom row, the
 *  bottom-left card's chip and the skill picker. */
export const BOTTOM_CHROME_CLEARANCE_PX = 112;
/** Space kept free below the top bar for widget zones. */
export const ZONE_TOP_CLEARANCE_PX = 140;

/** The grid's measures for a width class (`layout.json` `classes`): the
 *  column button edge, the shutter edge, the bottom-left card and the chrome
 *  scale (×1.33 on a television). Glove mode grows the touch targets. */
export interface GridMeasures {
  colPx: number;
  shutterPx: number;
  scale: number;
  card: { kind: "chip"; sizePx: number } | { kind: "card"; widthPx: number; heightPx: number };
  distanceOnly: boolean;
  defaultLevel: "full" | "route" | "off";
  focusRingsAlways: boolean;
  overlayFpsCap: number | null;
}

export function gridMeasures(width: WidthClass, glove: boolean): GridMeasures {
  const c = LAYOUT_CLASSES[width];
  const touch = glove ? GLOVE_TOUCH_SCALE : 1;
  const shutter = Math.max(TOUCH_TARGET.shutter, c.column_width);
  const blc = c.bottom_left_card;
  const extra = c as LayoutClassSpec & { focus_rings?: string; overlay_fps_cap?: number };
  return {
    colPx: Math.round(c.column_width * touch),
    shutterPx: Math.round(shutter * touch),
    scale: c.scale,
    card:
      blc.kind === "chip"
        ? { kind: "chip", sizePx: blc.size ?? 96 }
        : { kind: "card", widthPx: blc.width ?? 280, heightPx: blc.height ?? 180 },
    distanceOnly: c.labels === "distance_only",
    defaultLevel: c.default_layer_level,
    focusRingsAlways: extra.focus_rings === "always",
    overlayFpsCap: typeof extra.overlay_fps_cap === "number" ? extra.overlay_fps_cap : null,
  };
}

/** The video share of a split, and its bounds. */
const SPLIT_DIVIDER = viewsJson.views.find((v) => v.id === "split")?.divider ?? { default: 0.5, min: 0.3, max: 0.7 };
export const SPLIT_MIN = SPLIT_DIVIDER.min;
export const SPLIT_MAX = SPLIT_DIVIDER.max;
export const SPLIT_DEFAULT = SPLIT_DIVIDER.default;

export function clampSplit(ratio: number): number {
  if (!Number.isFinite(ratio)) return SPLIT_DEFAULT;
  return Math.min(SPLIT_MAX, Math.max(SPLIT_MIN, ratio));
}

/** Whether the on-screen thumb pads show: switched on, no controller
 *  connected, landscape, and a width class the spec lists
 *  (`layout.json` `on_screen_sticks.classes`). */
export function onscreenSticksShown(on: boolean, padLive: boolean, portrait: boolean, width: WidthClass): boolean {
  return on && !padLive && !portrait && (ON_SCREEN_STICKS.classes as readonly string[]).includes(width);
}

/** The thumb pad's edge (`on_screen_sticks.pad_size`). */
export const STICK_PAD_PX: number = ON_SCREEN_STICKS.pad_size;

const LEFT_COLUMN_LABELS = (layoutJson.zones as { id: string; labels?: { show_at: string[] } }[]).find((z) => z.id === "left_column")
  ?.labels;

/** Whether the left column's buttons carry their short label under the icon
 *  in this width class (`zones[left_column].labels.show_at`). */
export function leftColumnLabelsAt(width: WidthClass): boolean {
  return LEFT_COLUMN_LABELS?.show_at.includes(width) ?? false;
}

/** A built-in's short label for the left column (`builtin.short.<id>`). */
export function leftColumnCaption(builtinId: string): string {
  return specText(`builtin.short.${builtinId}`);
}

/** Touch targets grow by this factor in glove mode. */
export const GLOVE_TOUCH_SCALE = 1.25;

/** The framed screens' shell chrome sizes, as Tailwind classes over the CSS
 *  variables declared once in styles/globals.css (`--chrome-*`). The Feed
 *  draws none of it. */
export const CHROME = {
  /** The status strip's height. */
  stripHeight: "h-[var(--chrome-top)]",
  /** The utility (action) bar's height. */
  barHeight: "h-[var(--chrome-bottom)]",
  /** The menu: a rail in landscape, a strip in portrait. */
  menuSize: "landscape:w-[var(--chrome-rail)] portrait:h-[var(--chrome-strip)]",
} as const;

/** The flight screen's z-ladder, as Tailwind classes (literal here so the
 *  class scanner sees them). Readouts sit above the video and below
 *  everything the pilot touches; sheets and dialogs sit over all of it. */
export const Z = {
  video: "z-0",
  extOverlay: "z-10",
  marks: "z-[12]",
  hud: "z-[15]",
  zones: "z-20",
  pip: "z-[25]",
  zoneChrome: "z-30",
  topBar: "z-40",
  alerts: "z-[45]",
  sheets: "z-50",
} as const;
