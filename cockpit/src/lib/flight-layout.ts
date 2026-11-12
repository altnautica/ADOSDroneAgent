// The flight screen's zones as rectangles (cockpit spec `layout.json` zones)
// for one viewport: the top bar across the top with STOP at its right end,
// the left and right columns vertically centred in the band between the top
// bar and the bottom zones, the bottom-left card, the telemetry row and the
// skill picker along the bottom, the follow strip docked under the top bar
// between the columns, and the vision panel or the map pane beside the video.
// Every zone is placed from this one model, so a test can prove that no two
// zones overlap and that STOP is inside the viewport at every class. Sizes
// are CSS px from the chrome tokens; a television multiplies them by its
// class scale.

import { CHROME_TOKENS } from "@/lib/chrome-tokens";
import { leftColumnLabelsAt, type GridMeasures } from "@/lib/cockpit-layout";
import type { CockpitView } from "@/lib/cockpit-view";
import layoutJson from "@/shared/cockpit-spec/layout.json";
import { specText } from "@/shared/hud/spec-text";

export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** The right column's items, top to bottom (`layout.json` zone
 *  `right_column` `items`), by their spec ids. */
export type RightItem = (typeof RIGHT_ITEMS)[number];
export const RIGHT_ITEMS = ["shutter", "photo_video_switch", "zoom", "gimbal_pad", "camera_settings"] as const;
const RIGHT_ZONE = layoutJson.zones.find((z) => z.id === "right_column");
/** The items that leave the column, in order, where it is too short
 *  (`right_column.drop_order`). */
export const RIGHT_DROP_ORDER: readonly RightItem[] = (RIGHT_ZONE?.drop_order ?? []).filter((id): id is RightItem =>
  (RIGHT_ITEMS as readonly string[]).includes(id),
);

/** The top bar's items that move into the Menu when the row runs out of
 *  room, lowest priority first (`top_bar.overflow.to_menu_order`):
 *  `link_text` is the link's text, `link_icon` the link item as a whole. */
export type TopBarOverflowItem = "controller" | "nav_source" | "link_text" | "link_icon" | "layer_button";

interface TopBarSpec {
  overflow: { to_menu_order: TopBarOverflowItem[]; never_move: string[] };
  chip: { max_frac: number };
  link: { text: string; never: string; detail: string; icon_color: { ok_below_ms: number; warning_below_ms: number } };
  battery: { time_to_reserve: { text: string; short: string } };
}

const TOP_BAR_SPEC = layoutJson.zones.find((z) => z.id === "top_bar") as unknown as TopBarSpec;

export const TOP_BAR_OVERFLOW: readonly TopBarOverflowItem[] = TOP_BAR_SPEC.overflow.to_menu_order;

/** The top-bar items that never move: the flight-state chip, the status
 *  pill, battery, Menu (`settings`) and STOP. */
export const TOP_BAR_NEVER_MOVE: readonly string[] = TOP_BAR_SPEC.overflow.never_move;

/** The items in the Menu once `count` of them have moved, in move order. */
export function movedItems(count: number): Set<TopBarOverflowItem> {
  return new Set(TOP_BAR_OVERFLOW.slice(0, Math.max(0, count)));
}

/** Every movable item is in the Menu: the chip and the pill truncate with an
 *  ellipsis rather than push battery or STOP off the bar. */
export function barFull(count: number): boolean {
  return count >= TOP_BAR_OVERFLOW.length;
}

/** The widest the flight-state chip draws, as a fraction of the bar
 *  (`top_bar.chip.max_frac`); past it the text ends in an ellipsis and the
 *  full text shows on hover or tap (`chip.full_text`). */
export const CHIP_MAX_FRAC: number = TOP_BAR_SPEC.chip.max_frac;

/** The link cell (`top_bar.link`): the newest vehicle frame's age in the
 *  spec's words, and the icon's severity by its thresholds. */
export function linkCell(ageMs: number | null): { text: string; severity: "ok" | "warning" | "critical" } {
  const l = TOP_BAR_SPEC.link;
  if (ageMs === null || !Number.isFinite(ageMs)) return { text: specText(l.never), severity: "critical" };
  const s = ageMs / 1000;
  const severity = ageMs < l.icon_color.ok_below_ms ? "ok" : ageMs < l.icon_color.warning_below_ms ? "warning" : "critical";
  return { text: specText(l.text, { s: s < 10 ? s.toFixed(1) : String(Math.round(s)) }), severity };
}

/** The age of the newest vehicle frame the cockpit holds
 *  (`top_bar.link.metric`): the younger of the pilot frame and the last live
 *  telemetry sample. A pilot frame counts only while it reports the flight
 *  controller linked (`armed` not null): the onboard pilot keeps publishing
 *  while the FC is lost, and its frames prove nothing about the vehicle. The
 *  agent's heartbeat age is the fallback only, because a ground station's
 *  status carries none. Null with no frame at all. */
export function vehicleFrameAgeMs(i: {
  pilotAgeMs: number | null;
  pilotFcLinked: boolean;
  telemetryAgeMs: number | null;
  heartbeatAgeS: number | null | undefined;
}): number | null {
  const ages = [i.pilotFcLinked ? i.pilotAgeMs : null, i.telemetryAgeMs].filter((a): a is number => typeof a === "number" && Number.isFinite(a));
  if (ages.length > 0) return Math.max(0, Math.min(...ages));
  return typeof i.heartbeatAgeS === "number" && Number.isFinite(i.heartbeatAgeS) ? i.heartbeatAgeS * 1000 : null;
}

/** The link popover's detail line (RSSI), or null without a reading. */
export function linkDetail(rssiDbm: number | null | undefined): string | null {
  return typeof rssiDbm === "number" && Number.isFinite(rssiDbm) ? specText("cockpit.link.rssi", { v: `${Math.round(rssiDbm)} dBm` }) : null;
}

/** Time to the battery reserve (`top_bar.battery.time_to_reserve`): the
 *  short form while the bar is short of room. */
export function reserveText(t: string, short: boolean): string {
  const r = TOP_BAR_SPEC.battery.time_to_reserve;
  return specText(short ? r.short : r.text, { t });
}

/** How much of an alert level's hue the alert stack mixes into its dark
 *  panel, percent: at most 18, so the title keeps the panel's contrast over
 *  any picture. */
export const ALERT_TINT_PCT: Readonly<Record<"critical" | "warning" | "advisory", number>> = { critical: 18, warning: 14, advisory: 0 };

/** The smallest hit area of a flight-screen control, CSS px
 *  (`layout.json` `touch_target.min`). */
export const TOUCH_MIN_PX: number = layoutJson.touch_target.min;

/** Classes that grow a control's hit area to 48 × 48 CSS px (`TOUCH_MIN_PX`)
 *  around its drawing without moving anything: a transparent centred
 *  `::before`, part of the control for hit testing. The top bar is 48 px
 *  tall, so the hit area spans it. */
export const HIT_AREA =
  "relative before:absolute before:left-1/2 before:top-1/2 before:h-[48px] before:w-[max(100%,48px)] before:-translate-x-1/2 before:-translate-y-1/2 before:content-['']";

/** The class scale of a width class (`layout.json` `classes.<class>.scale`):
 *  text drawn outside a scaled zone is sized px × this. */
export function classScale(widthClass: keyof typeof layoutJson.classes): number {
  return layoutJson.classes[widthClass]?.scale ?? 1;
}

/** A label's size in CSS px outside a scaled zone: `base` × the class scale,
 *  never below the class's `label_min_px` (a television reads from across
 *  the room). */
export function labelPx(base: number, widthClass: keyof typeof layoutJson.classes): number {
  const cls = layoutJson.classes[widthClass];
  const min = cls && "label_min_px" in cls && typeof cls.label_min_px === "number" ? cls.label_min_px : 0;
  return Math.max(base * classScale(widthClass), min);
}

/** The Menu's row type size, CSS px: the chrome bar type scaled by the width
 *  class, so every Menu row (the entries, the Layers and overflow sections,
 *  the extension panels) reads at one size, a tv's from across the room. */
export function menuTextPx(widthClass: keyof typeof layoutJson.classes): number {
  return labelPx(CHROME_TOKENS.type.top_bar_px, widthClass);
}

/** Whether the agent has a video source to dial: its video reports running,
 *  or it lists a stream or a roster camera. A status that does not report a
 *  video state (none read yet, or a ground station relaying the drone's
 *  picture) counts as a source, so the dial goes ahead. */
export function videoHasSource(video: { state?: string | null; streams?: readonly unknown[] } | null | undefined, rosterCount: number): boolean {
  if (rosterCount > 0 || (video?.streams?.length ?? 0) > 0) return true;
  if (video?.state == null) return true;
  return video.state === "running";
}

/** The no-picture note in the spec's words (`video.state.*`), and whether it
 *  offers a retry; null while the picture plays. */
export function videoNote(state: "connecting" | "live" | "frozen" | "failed", hasSource = true): { text: string; retry: boolean } | null {
  if (!hasSource) return { text: specText("video.state.no_source"), retry: false };
  if (state === "connecting") return { text: specText("video.state.connecting"), retry: false };
  if (state === "frozen") return { text: specText("video.state.frozen"), retry: true };
  if (state === "failed") return { text: specText("video.state.failed"), retry: true };
  return null;
}

/** How far below the stage centre the no-picture note starts, in the centre
 *  marks' unit (the root font size, `1rem`): the reticle reaches 0.4 below
 *  the centre and the horizon mark at most `CENTRE_MARK_REACH_U`, so the note
 *  never sits under either. */
export const CENTRE_MARK_REACH_U = 4;
export const VIDEO_NOTE_OFFSET_U = CENTRE_MARK_REACH_U + 0.6;
export const VIDEO_NOTE_TOP = `calc(50% + ${VIDEO_NOTE_OFFSET_U}rem)`;

/** Unscaled CSS px. */
export const FLIGHT_PX = {
  edge: CHROME_TOKENS.edge_inset_px,
  barH: 48,
  barPad: 4,
  stopW: 84,
  /** The narrowest the never-moving top-bar items get: the chip without its
   *  secondary line, the pill as a dot and count, battery percent only, Menu
   *  and STOP, with the gaps between them. */
  barMinW: 112 + 44 + 64 + 44 + 84 + 5 * 6 + 2 * 4,
  colGap: 10,
  colGapMin: 4,
  touchMin: TOUCH_MIN_PX,
  /** The photo/video switch: one control that toggles, its hit area the
   *  touch floor high. */
  modeH: 48,
  modeW: 64,
  /** The gimbal pad's recentre knob: drawn 36 px, hit 48 px. */
  knob: 36,
  knobHit: 48,
  /** A left-column button's label line under its icon, and the width the
   *  label may take (medium, expanded and tv; compact shows icons only). */
  leftLabelH: 16,
  leftLabelW: 76,
  /** Two 48 px halves, the 1 px rule between them and the 2 px border. */
  zoomH: 100,
  gimbal: 108,
  rowH: 44,
  rowFullW: 368,
  rowShortW: 168,
  skillH: 48,
  skillW: { compact: 120, medium: 150, expanded: 180, tv: 180 },
  stripH: 56,
  divider: 12,
  vision: { min: 240, max: 380, share: 0.28 },
} as const;

export interface FlightLayoutInput {
  width: number;
  height: number;
  m: Pick<GridMeasures, "colPx" | "shutterPx" | "scale" | "card" | "distanceOnly">;
  /** The skill picker's width bucket. */
  widthClass: keyof typeof FLIGHT_PX.skillW;
  view: CockpitView;
  /** A node that flies: the left column, the bottom-left card and the
   *  telemetry row. */
  flies: boolean;
  /** The follow strip is up (a target designated, or following). */
  strip: boolean;
  /** The vehicle takes gimbal rates, so the pad can show. */
  gimbal: boolean;
  /** The video's share of a split. */
  splitRatio: number;
}

export interface FlightLayout {
  /** The surface the video (or, in the map view, the map) fills. */
  stage: Rect;
  /** What the bottom-left card shows: its instrument faces, or in the map
   *  view the video (`views.json` `map.video_slot`). */
  cardHoldsVideo: boolean;
  /** The map pane of a split, right of the divider. */
  mapPane: Rect | null;
  divider: Rect | null;
  visionPanel: Rect | null;
  topBar: Rect;
  stop: Rect;
  strip: Rect | null;
  /** Where alerts and flow overlays stack, top-centre. */
  alerts: Rect;
  leftColumn: Rect | null;
  rightColumn: Rect;
  rightItems: RightItem[];
  card: Rect | null;
  bottomRow: Rect | null;
  /** The telemetry row shows distance and height only. */
  bottomRowShort: boolean;
  skillPicker: Rect;
  /** The instruments' area: centred on the stage, clear of every zone. */
  hud: Rect;
  /** The height the columns centre in, between the top bar and the bottom
   *  zones. */
  bandHeight: number;
  /** The left column's button edge and gap, unscaled. */
  leftButtonPx: number;
  leftGapPx: number;
  /** The left column's buttons carry a short label under the icon. */
  leftLabels: boolean;
}

const right = (r: Rect) => r.x + r.w;
const bottom = (r: Rect) => r.y + r.h;

function rightItemHeight(item: RightItem, i: FlightLayoutInput): number {
  switch (item) {
    case "shutter":
      return i.m.shutterPx;
    case "photo_video_switch":
      return FLIGHT_PX.modeH;
    case "zoom":
      return FLIGHT_PX.zoomH;
    case "gimbal_pad":
      return FLIGHT_PX.gimbal;
    case "camera_settings":
      return i.m.colPx;
  }
}

function columnHeight(items: readonly RightItem[], i: FlightLayoutInput): number {
  return items.reduce((h, it) => h + rightItemHeight(it, i), 0) + FLIGHT_PX.colGap * Math.max(0, items.length - 1);
}

export function flightLayout(i: FlightLayoutInput): FlightLayout {
  const s = i.m.scale;
  const W = i.width;
  const H = i.height;
  const E = FLIGHT_PX.edge * s;

  // The video stage, and the panes beside it.
  let stage: Rect = { x: 0, y: 0, w: W, h: H };
  let mapPane: Rect | null = null;
  let divider: Rect | null = null;
  let visionPanel: Rect | null = null;
  const topBar: Rect = { x: E, y: E, w: W - 2 * E, h: FLIGHT_PX.barH * s };
  const y0 = bottom(topBar) + E;
  if (i.view === "split") {
    const videoW = Math.round(i.splitRatio * (W - FLIGHT_PX.divider));
    stage = { x: 0, y: 0, w: videoW, h: H };
    divider = { x: videoW, y: y0, w: FLIGHT_PX.divider, h: H - y0 };
    mapPane = { x: videoW + FLIGHT_PX.divider, y: 0, w: W - videoW - FLIGHT_PX.divider, h: H };
  } else if (i.view === "vision") {
    const panelW = Math.round(Math.min(FLIGHT_PX.vision.max, Math.max(FLIGHT_PX.vision.min, W * FLIGHT_PX.vision.share)));
    visionPanel = { x: W - E - panelW, y: y0, w: panelW, h: H - E - y0 };
    stage = { x: 0, y: 0, w: visionPanel.x, h: H };
  }

  const stopH = topBar.h - 2 * FLIGHT_PX.barPad * s;
  const stop: Rect = {
    x: right(topBar) - FLIGHT_PX.barPad * s - FLIGHT_PX.stopW * s,
    y: topBar.y + FLIGHT_PX.barPad * s,
    w: FLIGHT_PX.stopW * s,
    h: stopH,
  };

  // The bottom zones: the card bottom-left (its faces in the pilot and vision
  // views, the video in the map view; a split already shows the map), the
  // skill picker bottom-right, and the telemetry row between them, or raised
  // above them where it does not fit between.
  const showCard = i.flies && i.view !== "split";
  const cardW = (i.m.card.kind === "chip" ? i.m.card.sizePx : i.m.card.widthPx) * s;
  const cardH = (i.m.card.kind === "chip" ? i.m.card.sizePx : i.m.card.heightPx) * s;
  const card: Rect | null = showCard ? { x: stage.x + E, y: H - E - cardH, w: cardW, h: cardH } : null;
  const skillW = FLIGHT_PX.skillW[i.widthClass] * s;
  const skillPicker: Rect = { x: right(stage) - E - skillW, y: H - E - FLIGHT_PX.skillH * s, w: skillW, h: FLIGHT_PX.skillH * s };

  let bottomRow: Rect | null = null;
  let bottomRowShort = i.m.distanceOnly;
  const rowH = FLIGHT_PX.rowH * s;
  if (i.flies) {
    const lo = (card ? right(card) : stage.x) + E;
    const hi = skillPicker.x - E;
    const fullW = FLIGHT_PX.rowFullW * s;
    const shortW = FLIGHT_PX.rowShortW * s;
    const centred = (w: number, from: number, to: number) => Math.min(Math.max(stage.x + stage.w / 2 - w / 2, from), to - w);
    if (!bottomRowShort && hi - lo >= fullW) {
      bottomRow = { x: centred(fullW, lo, hi), y: H - E - rowH, w: fullW, h: rowH };
    } else if (hi - lo >= shortW && (bottomRowShort || stage.w - 2 * E < fullW)) {
      bottomRowShort = true;
      bottomRow = { x: centred(shortW, lo, hi), y: H - E - rowH, w: shortW, h: rowH };
    } else {
      // Raised above the bottom zones, centred on the stage.
      const w = !bottomRowShort && stage.w - 2 * E >= fullW ? fullW : shortW;
      bottomRowShort = w === shortW;
      const top = Math.min(card ? card.y : H, skillPicker.y) - E - rowH;
      bottomRow = { x: centred(w, stage.x + E, right(stage) - E), y: top, w, h: rowH };
    }
  }

  // The band the columns centre in.
  const bandTop = y0;
  const bandBottom = Math.min(card ? card.y : H, skillPicker.y, bottomRow ? bottomRow.y : H) - E;
  const bandH = Math.max(0, bandBottom - bandTop);

  // Left column: icon plus a short label under it at medium and up, icon
  // only at compact. Its buttons shrink (never under the 48 px touch floor)
  // where a short band cannot hold three glove-size buttons and their gaps;
  // where even that is too tall, the labels go.
  let leftColumn: Rect | null = null;
  let leftButtonPx = i.m.colPx;
  let leftGapPx: number = FLIGHT_PX.colGap;
  let leftLabels = leftColumnLabelsAt(i.widthClass);
  if (i.flies) {
    const fit = bandH / s;
    const need = () => 3 * (leftButtonPx + (leftLabels ? FLIGHT_PX.leftLabelH : 0)) + 2 * leftGapPx;
    if (need() > fit) leftGapPx = FLIGHT_PX.colGapMin;
    if (need() > fit && leftLabels && 3 * (FLIGHT_PX.touchMin + FLIGHT_PX.leftLabelH) + 2 * leftGapPx > fit) leftLabels = false;
    const label = leftLabels ? FLIGHT_PX.leftLabelH : 0;
    if (need() > fit) leftButtonPx = Math.max(FLIGHT_PX.touchMin, Math.floor((fit - 2 * leftGapPx) / 3 - label));
    const w = Math.max(leftButtonPx, leftLabels ? FLIGHT_PX.leftLabelW : 0) * s;
    const h = need() * s;
    leftColumn = { x: stage.x + E, y: bandTop + Math.max(0, (bandH - h) / 2), w, h };
  }

  // Right column items (`right_column.items`), leaving in `drop_order` (the
  // gimbal pad, then zoom) where the band is too short for them.
  const rightItems: RightItem[] = RIGHT_ITEMS.filter((it) => it !== "gimbal_pad" || i.gimbal);
  for (const drop of RIGHT_DROP_ORDER) {
    if (columnHeight(rightItems, i) * s <= bandH) break;
    const at = rightItems.indexOf(drop);
    if (at >= 0) rightItems.splice(at, 1);
  }
  const rightW = Math.max(i.m.shutterPx, i.m.colPx, FLIGHT_PX.modeW, rightItems.includes("gimbal_pad") ? FLIGHT_PX.gimbal : 0) * s;
  const rightH = columnHeight(rightItems, i) * s;
  const rightColumn: Rect = { x: right(stage) - E - rightW, y: bandTop + Math.max(0, (bandH - rightH) / 2), w: rightW, h: rightH };

  // Between the columns: the follow strip, then the alerts.
  const midX = (leftColumn ? right(leftColumn) : stage.x) + E;
  const midW = rightColumn.x - E - midX;
  const strip: Rect | null = i.strip ? { x: midX, y: y0, w: midW, h: FLIGHT_PX.stripH * s } : null;
  const alerts: Rect = { x: midX, y: (strip ? bottom(strip) : bandTop) + E, w: midW, h: 0 };

  // The instruments keep clear of every zone and stay centred on the stage,
  // so the centre marks sit on the video's centre.
  const insetX = Math.max((leftColumn ? right(leftColumn) : stage.x) - stage.x, right(stage) - rightColumn.x) + E;
  const insetY = Math.max(strip ? bottom(strip) + E : y0, H - bandBottom);
  const hud: Rect = { x: stage.x + insetX, y: insetY, w: Math.max(0, stage.w - 2 * insetX), h: Math.max(0, H - 2 * insetY) };

  return {
    stage,
    cardHoldsVideo: card !== null && i.view === "map",
    mapPane,
    divider,
    visionPanel,
    topBar,
    stop,
    strip,
    alerts,
    leftColumn,
    rightColumn,
    rightItems,
    card,
    bottomRow,
    bottomRowShort,
    skillPicker,
    hud,
    bandHeight: bandH,
    leftButtonPx,
    leftGapPx,
    leftLabels,
  };
}

/** Whether two rectangles share any area. */
export function intersects(a: Rect, b: Rect): boolean {
  return a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
}

/** CSS for a zone placed at a rectangle. */
export function rectStyle(r: Rect): { left: number; top: number; width: number; height: number } {
  return { left: r.x, top: r.y, width: r.w, height: r.h };
}
