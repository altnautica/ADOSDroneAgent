/**
 * @module hud/target-layout
 * @description Screen geometry for the HUD target layer: maps resolver items
 * (frame px) onto the rendered video rect, places the top and bottom tags,
 * hides overlapping lower-priority tags, and derives the reticle, feet tick,
 * search ring, off-frame arrow, ghost path, hit areas and focus order. Pure,
 * so every surface that draws targets lays them out identically.
 *
 * @license GPL-3.0-only
 */

import {
  bracketArm,
  bracketPath,
  offFrameArrow,
  type DrawItem,
  type FrameRect,
  type ResolvedTargets,
  type TagTone,
  type TargetKind,
} from "./target-state";

/** The letterbox-corrected rendered video rect, in CSS px relative to the
 * overlay wrapper's top-left (07 §3). */
export interface RenderedRect {
  left: number;
  top: number;
  width: number;
  height: number;
}

export const TAG_PAD = 6;
export const TAG_OFFSET = 3;
export const TAG_GAP = 4;
/** Mono advance (0.6 em) plus the 0.06 em tag tracking. */
export const TAG_CHAR_EM = 0.66;
export const CANDIDATE_TAG_MIN_H = 24;
export const HIT_MIN = 44;
export const RETICLE_GAP = 6;
export const RETICLE_ARM = 18;
export const FEET_TICK = 10;
export const ARROW_LEN = 14;
export const ARROW_INSET = 16;
export const SEARCH_RING_MIN = 24;
export const SEARCH_RING_RATIO = 0.6;
export const GHOST_WINDOW_MS = 1000;
export const GHOST_MAX = 30;
export const FOCUS_OFFSET = 3;

export type TargetWidthClass = "compact" | "medium" | "expanded" | "tv";

export interface ScreenRect {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface Point {
  x: number;
  y: number;
}

export interface PlacedTag {
  text: string;
  tone: TagTone;
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface GlyphLayout {
  item: DrawItem;
  /** Unique within one layout (resolver key, de-duplicated). */
  key: string;
  kind: TargetKind;
  box: ScreenRect | null;
  brackets: string | null;
  topTag: PlacedTag | null;
  bottomTags: PlacedTag[];
  reticle: string | null;
  feet: string | null;
  ring: { cx: number; cy: number; r: number } | null;
  arrow: { x: number; y: number; path: string } | null;
  /** Click target (detection-backed items only). */
  hit: ScreenRect | null;
  ghost: Point[];
  fontPx: number;
}

/** A followed-box bottom-centre sample in frame px. */
export interface GhostSample {
  t: number;
  x: number;
  y: number;
}

export interface LayoutOptions {
  rendered: RenderedRect;
  fontPx: number;
  widthClass: TargetWidthClass;
  /** Items whose candidate tag shows even when the box is short. */
  revealKeys?: ReadonlySet<string>;
  /** The on-screen frame rect (box smoothing); defaults to `item.rect`. */
  displayRect?: (item: DrawItem) => FrameRect | null;
  /** Followed-box history for the COASTING ghost path. */
  ghost?: readonly GhostSample[];
}

/** Kinds drawn with corner brackets. */
const BRACKETED: Record<TargetKind, boolean> = {
  CANDIDATE: false,
  TRACKED: true,
  LOCKED: true,
  ACQUIRING: true,
  FOLLOWING: true,
  HOLDING: true,
  COASTING: true,
  SEARCHING: false,
  SEARCHING_OFFFRAME: false,
  LOST: true,
  GT: false,
};

/** Width class from the container width. */
export function targetWidthClass(width: number): TargetWidthClass {
  if (width < 600) return "compact";
  if (width < 1024) return "medium";
  if (width < 1600) return "expanded";
  return "tv";
}

/** Tag type size: 9 px in the PiP, 13 px at `tv`, 11 px otherwise. */
export function tagFontPx(widthClass: TargetWidthClass, pip: boolean): number {
  if (pip) return 9;
  return widthClass === "tv" ? 13 : 11;
}

/** `object-cover` placement of a frame inside a wrapper. */
export function coverRect(wrapW: number, wrapH: number, frameW: number, frameH: number): RenderedRect {
  if (frameW <= 0 || frameH <= 0 || wrapW <= 0 || wrapH <= 0) {
    return { left: 0, top: 0, width: wrapW, height: wrapH };
  }
  const scale = Math.max(wrapW / frameW, wrapH / frameH);
  const width = frameW * scale;
  const height = frameH * scale;
  return { left: (wrapW - width) / 2, top: (wrapH - height) / 2, width, height };
}

export function frameToScreen(r: FrameRect, frameW: number, frameH: number, rr: RenderedRect): ScreenRect {
  const sx = frameW > 0 ? rr.width / frameW : 0;
  const sy = frameH > 0 ? rr.height / frameH : 0;
  return { x: rr.left + r.x * sx, y: rr.top + r.y * sy, w: r.w * sx, h: r.h * sy };
}

export function tagSize(text: string, fontPx: number): { w: number; h: number } {
  return { w: Math.ceil(text.length * fontPx * TAG_CHAR_EM + 2 * TAG_PAD), h: fontPx + TAG_PAD };
}

function clampX(x: number, w: number, b: RenderedRect): number {
  const max = b.left + b.width - w;
  return max < b.left ? b.left : Math.min(max, Math.max(b.left, x));
}

/** Top tag: above-left of the box, inside the box when that leaves the
 * rendered rect, clamped horizontally. */
export function placeTopTag(box: ScreenRect, size: { w: number; h: number }, b: RenderedRect): Point {
  let y = box.y - TAG_OFFSET - size.h;
  if (y < b.top) y = box.y + TAG_OFFSET;
  return { x: clampX(box.x, size.w, b), y };
}

/** Bottom tags: centred under the box, 4 px apart, inside the box bottom when
 * below would leave the rendered rect. */
export function placeBottomTags(
  box: ScreenRect,
  sizes: readonly { w: number; h: number }[],
  b: RenderedRect,
): Point[] {
  if (sizes.length === 0) return [];
  const total = sizes.reduce((s, z) => s + z.w, 0) + TAG_GAP * (sizes.length - 1);
  const h = Math.max(...sizes.map((z) => z.h));
  let y = box.y + box.h + TAG_OFFSET;
  if (y + h > b.top + b.height) y = box.y + box.h - TAG_OFFSET - h;
  let x = clampX(box.x + box.w / 2 - total / 2, total, b);
  return sizes.map((z) => {
    const p = { x, y };
    x += z.w + TAG_GAP;
    return p;
  });
}

export function rectsOverlap(a: ScreenRect, b: ScreenRect): boolean {
  return a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
}

/** Hit area: the box inflated about its centre to at least `min` square. */
export function inflateHit(box: ScreenRect, min = HIT_MIN): ScreenRect {
  const w = Math.max(box.w, min);
  const h = Math.max(box.h, min);
  return { x: box.x + box.w / 2 - w / 2, y: box.y + box.h / 2 - h / 2, w, h };
}

/** Centre reticle; null when the box is smaller than `2·(gap+arm)`. */
export function reticlePath(box: ScreenRect, gap = RETICLE_GAP, arm = RETICLE_ARM): string | null {
  const span = 2 * (gap + arm);
  if (box.w < span || box.h < span) return null;
  const cx = box.x + box.w / 2;
  const cy = box.y + box.h / 2;
  return [
    `M ${cx},${cy - gap} V ${cy - gap - arm}`,
    `M ${cx + gap},${cy} H ${cx + gap + arm}`,
    `M ${cx},${cy + gap} V ${cy + gap + arm}`,
    `M ${cx - gap},${cy} H ${cx - gap - arm}`,
  ].join(" ");
}

export function feetTickPath(box: ScreenRect): string {
  const cx = box.x + box.w / 2;
  const y = box.y + box.h;
  return `M ${cx - FEET_TICK / 2},${y} H ${cx + FEET_TICK / 2}`;
}

/** Search ring at the predicted feet point, radius `max(24, 0.6·h)`. */
export function searchRing(box: ScreenRect): { cx: number; cy: number; r: number } {
  return {
    cx: box.x + box.w / 2,
    cy: box.y + box.h,
    r: Math.max(SEARCH_RING_MIN, SEARCH_RING_RATIO * box.h),
  };
}

/** The 90° sweep arc, from 12 o'clock clockwise to 3 o'clock. */
export function sweepArcPath(cx: number, cy: number, r: number): string {
  return `M ${cx},${cy - r} A ${r},${r} 0 0 1 ${cx + r},${cy}`;
}

function direction(relDeg: number): Point {
  const rad = (relDeg * Math.PI) / 180;
  return { x: Math.sin(rad), y: -Math.cos(rad) };
}

/** Isosceles triangle centred on `(x, y)`, tip pointing along `relDeg`
 * (screen angle, clockwise from up). */
export function arrowPath(x: number, y: number, relDeg: number, len = ARROW_LEN): string {
  const d = direction(relDeg);
  const n = { x: -d.y, y: d.x };
  const half = len / 2;
  const wing = len * 0.4;
  const tip = { x: x + d.x * half, y: y + d.y * half };
  const base = { x: x - d.x * half, y: y - d.y * half };
  const f = (v: number) => Math.round(v * 100) / 100;
  return (
    `M ${f(tip.x)},${f(tip.y)} ` +
    `L ${f(base.x + n.x * wing)},${f(base.y + n.y * wing)} ` +
    `L ${f(base.x - n.x * wing)},${f(base.y - n.y * wing)} Z`
  );
}

/** Top-left of a tag sitting inward of an off-frame arrow, clamped into the
 * rendered rect. */
export function arrowTagPos(
  x: number,
  y: number,
  relDeg: number,
  size: { w: number; h: number },
  b: RenderedRect,
  len = ARROW_LEN,
): Point {
  const d = direction(relDeg);
  const reach = len / 2 + TAG_GAP + Math.abs(d.x) * (size.w / 2) + Math.abs(d.y) * (size.h / 2);
  const cx = x - d.x * reach;
  const cy = y - d.y * reach;
  const top = Math.min(b.top + b.height - size.h, Math.max(b.top, cy - size.h / 2));
  return { x: clampX(cx - size.w / 2, size.w, b), y: top };
}

/** Append a followed-box sample, keeping the last 1 s and at most 30. */
export function pushGhostSample(samples: readonly GhostSample[], s: GhostSample): GhostSample[] {
  const next = samples.filter((p) => p.t >= s.t - GHOST_WINDOW_MS && p.t <= s.t);
  next.push(s);
  return next.length > GHOST_MAX ? next.slice(next.length - GHOST_MAX) : next;
}

function topTagText(item: DrawItem, widthClass: TargetWidthClass): string | null {
  if (!item.topTag) return null;
  if (widthClass === "compact") {
    if (item.kind === "CANDIDATE") return null;
    if (item.kind === "TRACKED" && item.trackId !== null) return `#${item.trackId}`;
  }
  return item.topTag.text;
}

/** Lay out every resolved item over the rendered video rect. */
export function layoutTargets(resolved: ResolvedTargets, o: LayoutOptions): GlyphLayout[] {
  const { rendered: rr, fontPx } = o;
  const out: GlyphLayout[] = [];
  const seen = new Set<string>();
  const shownTopTags: ScreenRect[] = [];

  resolved.items.forEach((item, i) => {
    const key = seen.has(item.key) ? `${item.key}:${i}` : item.key;
    seen.add(key);
    const frameRect = (item.rect && o.displayRect?.(item)) ?? item.rect;
    const box = frameRect ? frameToScreen(frameRect, resolved.frameW, resolved.frameH, rr) : null;

    const g: GlyphLayout = {
      item,
      key,
      kind: item.kind,
      box,
      brackets: null,
      topTag: null,
      bottomTags: [],
      reticle: null,
      feet: null,
      ring: null,
      arrow: null,
      hit: item.detection && box ? inflateHit(box) : null,
      ghost: [],
      fontPx,
    };

    // Anchor rect the top tag sits against.
    let anchor: ScreenRect | null = box;
    if (box && BRACKETED[item.kind]) {
      g.brackets = bracketPath(box.x, box.y, box.w, box.h, bracketArm(box.w, box.h));
    }
    if (box && item.reticle) g.reticle = reticlePath(box);
    if (box && item.feetTick) g.feet = feetTickPath(box);
    if (box && item.kind === "SEARCHING") {
      g.ring = searchRing(box);
      anchor = { x: g.ring.cx - g.ring.r, y: g.ring.cy - g.ring.r, w: 2 * g.ring.r, h: 2 * g.ring.r };
    }
    if (box && item.kind === "COASTING" && o.ghost && o.ghost.length > 0) {
      const sx = resolved.frameW > 0 ? rr.width / resolved.frameW : 0;
      const sy = resolved.frameH > 0 ? rr.height / resolved.frameH : 0;
      g.ghost = o.ghost.map((p) => ({ x: rr.left + p.x * sx, y: rr.top + p.y * sy }));
      g.ghost.push({ x: box.x + box.w / 2, y: box.y + box.h });
    }

    const text = topTagText(item, o.widthClass);
    const tone = item.topTag?.tone ?? "ink";
    if (item.kind === "SEARCHING_OFFFRAME" && item.relBearingDeg !== undefined) {
      const p = offFrameArrow(item.relBearingDeg, rr, ARROW_INSET);
      g.arrow = { x: p.x, y: p.y, path: arrowPath(p.x, p.y, item.relBearingDeg) };
      if (text) {
        const size = tagSize(text, fontPx);
        const at = arrowTagPos(p.x, p.y, item.relBearingDeg, size, rr);
        g.topTag = { text, tone, ...at, ...size };
      }
    } else if (anchor && text) {
      const hideShort =
        item.kind === "CANDIDATE" && anchor.h < CANDIDATE_TAG_MIN_H && !o.revealKeys?.has(key);
      if (!hideShort) {
        const size = tagSize(text, fontPx);
        g.topTag = { text, tone, ...placeTopTag(anchor, size, rr), ...size };
      }
    }

    if (g.topTag) {
      if (shownTopTags.some((r) => rectsOverlap(r, g.topTag!))) g.topTag = null;
      else shownTopTags.push(g.topTag);
    }

    if (box && item.bottomTags.length > 0) {
      const sizes = item.bottomTags.map((t) => tagSize(t.text, fontPx));
      const at = placeBottomTags(box, sizes, rr);
      g.bottomTags = item.bottomTags.map((t, k) => ({ ...t, ...at[k], ...sizes[k] }));
    }
    out.push(g);
  });
  return out;
}
