// Width classes and named cockpit layout measures, so each clearance is
// defined once. Mirrors the Android cockpit (`CockpitLayout.kt` and the
// Skill Bar's width classes): the same breakpoints, clearances and inset
// sizing, so a pilot moving between the apps sees the same arrangement.

/** The window width class. `tv` is a large panel or the HDMI kiosk. */
export type WidthClass = "compact" | "medium" | "expanded" | "tv";

export const MEDIUM_MIN_PX = 600;
export const EXPANDED_MIN_PX = 1024;
export const TV_MIN_PX = 1600;

/** The width class for a viewport `widthPx` wide; the kiosk is always `tv`. */
export function widthClassFor(widthPx: number, kiosk: boolean): WidthClass {
  if (kiosk || widthPx >= TV_MIN_PX) return "tv";
  if (widthPx >= EXPANDED_MIN_PX) return "expanded";
  if (widthPx >= MEDIUM_MIN_PX) return "medium";
  return "compact";
}

/** The Feed arrangement: `compact` keeps the video full-bleed with a minimap
 *  inset, `split` puts video and map side by side behind a draggable divider,
 *  `portrait` stacks a 16:9 video over the map. */
export type FeedLayout = "compact" | "split" | "portrait";

/** Which Feed layout a window gets. Without a map to show (a node that does
 *  not fly), a landscape window keeps the full-bleed video. */
export function feedLayoutFor(width: WidthClass, portrait: boolean, hasMap: boolean): FeedLayout {
  if (portrait) return "portrait";
  if (width === "compact" || !hasMap) return "compact";
  return "split";
}

/** Space kept free above the bottom edge for the Skill Bar. */
export const SKILL_BAR_CLEARANCE_PX = 112;
/** Space kept free below the safety band for the inset row (minimap, tabs). */
export const ZONE_TOP_CLEARANCE_PX = 140;

/** The minimap inset's width on the compact layout: 26 % of the Feed's width,
 *  kept between 160 and 280 px. A CSS length, so the browser resolves the
 *  percentage against the stage as it resizes. */
export const INSET_WIDTH = "clamp(160px, 26%, 280px)";
/** The minimap inset's width-to-height ratio. */
export const INSET_ASPECT = 16 / 10;

/** The video share of a split, and its bounds. */
export const SPLIT_MIN = 0.2;
export const SPLIT_MAX = 0.8;
export const SPLIT_DEFAULT = 0.6;

export function clampSplit(ratio: number): number {
  if (!Number.isFinite(ratio)) return SPLIT_DEFAULT;
  return Math.min(SPLIT_MAX, Math.max(SPLIT_MIN, ratio));
}

/** Touch targets grow by this factor in glove mode. */
export const GLOVE_TOUCH_SCALE = 1.25;

/** A Skill Bar slot's edge in px for a width class, scaled up in glove mode. */
export function skillSlotPx(width: WidthClass, glove: boolean): number {
  const base = width === "compact" ? 64 : width === "medium" ? 72 : 88;
  return glove ? base * GLOVE_TOUCH_SCALE : base;
}

/** The floating shell chrome's sizes, as Tailwind classes over the CSS
 *  variables declared once in styles/globals.css (`--chrome-*`). The chrome
 *  takes exactly these sizes and the Feed keeps clear of exactly these, so
 *  the two can never disagree. */
export const CHROME = {
  /** The status strip's height. */
  stripHeight: "h-[var(--chrome-top)]",
  /** The utility (action) bar's height. */
  barHeight: "h-[var(--chrome-bottom)]",
  /** The menu: a rail in landscape, a strip in portrait. */
  menuSize: "landscape:w-[var(--chrome-rail)] portrait:h-[var(--chrome-strip)]",
  /** The Feed region inside the chrome. */
  regionTop: "top-[var(--chrome-top)]",
  regionBottom: "bottom-[var(--chrome-bottom)] portrait:bottom-[calc(var(--chrome-bottom)+var(--chrome-strip))]",
  regionRight: "right-[var(--chrome-gap)]",
  regionLeftCollapsed: "left-[var(--chrome-gap)]",
  regionLeftBesideRail: "left-[var(--chrome-gap)] landscape:left-[calc(var(--chrome-rail)+var(--chrome-gap))]",
} as const;

/** The CS-1 z-ladder, as Tailwind classes (literal here so the class scanner
 *  sees them). Readouts sit above the video and below everything the pilot
 *  touches; sheets and dialogs sit over all of it. */
export const Z = {
  video: "z-0",
  extOverlay: "z-10",
  marks: "z-[12]",
  hud: "z-[15]",
  zones: "z-20",
  pip: "z-[25]",
  skillBar: "z-30",
  band: "z-40",
  alerts: "z-[45]",
  sheets: "z-50",
} as const;
