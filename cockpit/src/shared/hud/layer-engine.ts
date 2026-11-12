/**
 * @module hud/layer-engine
 * @description The frame loop behind a video's data layers: one canvas, a
 * plain mutable frame store (never React state per frame) and a
 * change-driven requestAnimationFrame loop that repaints only when a new
 * frame arrived, an interpolation is still running, the canvas resized, the
 * configuration changed or a staleness threshold passed. Keyed items are
 * interpolated between the two newest `ar` frames over the measured
 * inter-frame interval, so 10–20 Hz data moves smoothly at display refresh.
 *
 * Staleness (cockpit-spec layers.json), per layer: a layer whose last receipt
 * is older than 500 ms dims, older than 2 s it hides and an "<layer>
 * unavailable" chip shows. Each layer has its own draw buffer and alpha, so a
 * dead depth stream never rides on fresh obstacles, nor the reverse.
 *
 * A keep-alive (the same `ts_ms` again, sent by the agent about once a second
 * while a scene is unchanged) refreshes only the receipt time: it neither
 * restarts interpolation nor replaces the previous frame.
 *
 * `fpsCap` limits paints (30 on the kiosk / TV class). `stats()` reports the
 * paint durations for the overlay budget checks.
 *
 * Dependency-free apart from its siblings: Mission Control copies it.
 *
 * @license GPL-3.0-only
 */

import { arStatusChip, layoutAr, type LayerLevel } from "./ar-layout";
import { decodeBase64Into, type AvoidState, type MissionState } from "./cockpit-frames";
import {
  DrawOpBuffer,
  layoutCells,
  layoutSectorArc,
  layoutZones,
  lerpOps,
  type Rect,
} from "./layer-layout";
import tokensJson from "../cockpit-spec/tokens.json";
import { setupText } from "../setup-wizard/strings";
import { paintOps } from "./layer-paint";
import { LayerFrameStore } from "./layer-frame-store";

export { LayerFrameStore };

export const DIM_AFTER_MS = 500;
export const HIDE_AFTER_MS = 2000;
/** Interpolation interval bounds (ms): a gap outside snaps. */
const MIN_INTERVAL_MS = 30;
const MAX_INTERVAL_MS = 250;
const FADE_MS = 120;
const OBSTACLE_GRID = { maxM: 5, nearM: 2, mode: "solid" } as const;

export interface LayerConfig {
  cameraId: string | null;
  /** Effective layer ids (after level, flags and view). */
  layers: ReadonlySet<string>;
  level: LayerLevel;
  /** Video intrinsic size, used when no `ar` frame gives one. */
  videoW: number;
  videoH: number;
  mission: MissionState | null;
  avoid: AvoidState | null;
  showLoaded: boolean;
  /** Depth view options: range Off / 2 m / 5 m, Solid / Outline. */
  depthRangeM: 0 | 2 | 5;
  depthMode: "solid" | "outline";
  /** Paint cap, frames per second (0 = display rate). */
  fpsCap: number;
  /** When set, the status chips are not painted on the canvas: the engine
   *  reports them here instead (one bit per {@link CHIP_IDS} entry, called
   *  only when the set changes, and with 0 on `stop`), so the host can stack
   *  them with its own banners where nothing covers them. */
  onChips?: (mask: number) => void;
}

export interface PaintStats {
  frames: number;
  p50Ms: number;
  p95Ms: number;
  maxMs: number;
}

const STAT_RING = 600;

type Visibility = "fresh" | "dim" | "hidden";

function visibility(at: number, has: boolean, now: number): Visibility {
  if (!has) return "hidden";
  const age = now - at;
  return age > HIDE_AFTER_MS ? "hidden" : age > DIM_AFTER_MS ? "dim" : "fresh";
}

const VIS_CODE: Readonly<Record<Visibility, number>> = { fresh: 0, dim: 1, hidden: 2 };

/** The status chip colour (tokens.json `layer.unavailable_chip`). */
export const UNAVAILABLE_CHIP: string = tokensJson.layer.unavailable_chip;

/** Status chip texts from the spec strings, resolved once. */
export const CHIP_TEXT = {
  depth: setupText("layer.unavailable", { layer: "Depth" }),
  depthUnscaled: setupText("layer.depth_unscaled"),
  surfaces: setupText("layer.unavailable", { layer: "Surfaces" }),
  obstacles: setupText("layer.unavailable", { layer: "Obstacles" }),
  ar: setupText("layer.unavailable", { layer: "AR" }),
  arApprox: setupText("layer.ar_approx"),
  targets: setupText("layer.unavailable", { layer: "Targets" }),
} as const;

export type ChipId = keyof typeof CHIP_TEXT;
/** Status chips in stacking order; chip `CHIP_IDS[i]` is bit `1 << i`. */
export const CHIP_IDS: readonly ChipId[] = ["depth", "depthUnscaled", "surfaces", "obstacles", "ar", "arApprox", "targets"];

/** The chip texts of a reported mask, in stacking order. */
export function chipTexts(mask: number): string[] {
  return CHIP_IDS.filter((_, i) => (mask & (1 << i)) !== 0).map((id) => CHIP_TEXT[id]);
}

export class LayerEngine {
  private canvas: HTMLCanvasElement | null = null;
  private ctx: CanvasRenderingContext2D | null = null;
  private raf = 0;
  private running = false;
  private dirty = true;
  private seen = -1;
  private cfg: LayerConfig;
  private cssW = 0;
  private cssH = 0;
  private dpr = 1;
  private lastPaint = 0;
  private lastVis = -1;
  private arLaidVersion = -1;
  private depthLaid = -1;
  private surfLaid = -1;
  private obstLaid = -1;
  private interpUntil = 0;

  private arPrevOps = new DrawOpBuffer();
  private arNextOps = new DrawOpBuffer();
  private arOut = new DrawOpBuffer();
  private depthOps = new DrawOpBuffer();
  private surfOps = new DrawOpBuffer();
  private obstOps = new DrawOpBuffer();
  private chipOps = new DrawOpBuffer();
  /** Next chip position (CSS px), reset per paint. */
  private chipX = 0;
  private chipY = 0;
  /** This paint's chips, and the set last given to `onChips`. */
  private chipMask = 0;
  private chipsReported = 0;
  /** Reused video rects: the default frame (chips, zones, sectors) and one
   *  per AR frame size while laying out. */
  private rectMain: Rect = { x: 0, y: 0, w: 0, h: 0 };
  private rectFrame: Rect = { x: 0, y: 0, w: 0, h: 0 };
  private nearBuf = new Uint8Array(0);
  private measBuf = new Uint8Array(0);
  private paintMs = new Float64Array(STAT_RING);
  private paintN = 0;

  constructor(
    readonly store: LayerFrameStore,
    cfg: LayerConfig,
  ) {
    this.cfg = cfg;
  }

  attach(canvas: HTMLCanvasElement): void {
    this.canvas = canvas;
    this.ctx = canvas.getContext("2d");
    this.invalidate();
  }

  configure(patch: Partial<LayerConfig>): void {
    this.cfg = { ...this.cfg, ...patch };
    this.arLaidVersion = -1;
    this.relayCells();
    this.invalidate();
  }

  /** Canvas box changed size (CSS px) or device pixel ratio. */
  resize(cssW: number, cssH: number, dpr: number): void {
    if (cssW === this.cssW && cssH === this.cssH && dpr === this.dpr) return;
    this.cssW = cssW;
    this.cssH = cssH;
    this.dpr = dpr;
    if (this.canvas) {
      this.canvas.width = Math.max(1, Math.round(cssW * dpr));
      this.canvas.height = Math.max(1, Math.round(cssH * dpr));
    }
    this.arLaidVersion = -1;
    this.relayCells();
    this.invalidate();
  }

  private relayCells(): void {
    this.depthLaid = -1;
    this.surfLaid = -1;
    this.obstLaid = -1;
  }

  invalidate(): void {
    this.dirty = true;
  }

  start(): void {
    if (this.running || typeof requestAnimationFrame === "undefined") return;
    this.running = true;
    this.raf = requestAnimationFrame(this.tick);
  }

  stop(): void {
    this.running = false;
    if (this.raf) cancelAnimationFrame(this.raf);
    this.raf = 0;
    this.ctx?.clearRect(0, 0, this.canvas?.width ?? 0, this.canvas?.height ?? 0);
    this.reportChips(0);
  }

  private reportChips(mask: number): void {
    const on = this.cfg.onChips;
    if (!on || mask === this.chipsReported) return;
    this.chipsReported = mask;
    on(mask);
  }

  stats(): PaintStats {
    const n = Math.min(this.paintN, STAT_RING);
    if (n === 0) return { frames: 0, p50Ms: 0, p95Ms: 0, maxMs: 0 };
    const sorted = Array.from(this.paintMs.subarray(0, n)).sort((a, b) => a - b);
    return {
      frames: this.paintN,
      p50Ms: sorted[Math.floor(n * 0.5)],
      p95Ms: sorted[Math.min(n - 1, Math.floor(n * 0.95))],
      maxMs: sorted[n - 1],
    };
  }

  resetStats(): void {
    this.paintN = 0;
  }

  private tick = (now: number): void => {
    if (!this.running) return;
    this.raf = requestAnimationFrame(this.tick);
    const cap = this.cfg.fpsCap;
    if (cap > 0 && now - this.lastPaint < 1000 / cap - 1) return;
    const vis = this.visKey(now);
    const changed = this.store.version !== this.seen || vis !== this.lastVis;
    if (!this.dirty && !changed && now > this.interpUntil) return;
    this.seen = this.store.version;
    this.lastVis = vis;
    this.dirty = false;
    this.lastPaint = now;
    const t0 = performance.now();
    this.paint(now);
    this.paintMs[this.paintN % STAT_RING] = performance.now() - t0;
    this.paintN++;
  };

  /** A number that changes exactly when some layer crosses a staleness step
   *  (two bits per layer), so an idle tick allocates nothing. */
  private visKey(now: number): number {
    const s = this.store;
    return (
      VIS_CODE[visibility(s.ar.seen, s.ar.data !== null, now)] |
      (VIS_CODE[visibility(s.obstacles.seen, s.obstacles.data !== null, now)] << 2) |
      (VIS_CODE[visibility(s.depth.seen, s.depth.data !== null, now)] << 4) |
      (VIS_CODE[visibility(s.surfaces.seen, s.surfaces.data !== null, now)] << 6)
    );
  }

  /** The letterboxed video rect of a `fw`×`fh` frame (0 = the configured
   *  size), written into `out`. */
  private videoRect(out: Rect, fw: number, fh: number): Rect {
    const bw = this.cssW;
    const bh = this.cssH;
    const w0 = fw || this.cfg.videoW;
    const h0 = fh || this.cfg.videoH;
    if (w0 <= 0 || h0 <= 0 || bw <= 0 || bh <= 0) {
      out.x = 0;
      out.y = 0;
      out.w = bw;
      out.h = bh;
      return out;
    }
    const k = Math.min(bw / w0, bh / h0);
    out.w = w0 * k;
    out.h = h0 * k;
    out.x = (bw - out.w) / 2;
    out.y = (bh - out.h) / 2;
    return out;
  }

  /** Stack one status chip centred under the top chrome. */
  private chip(id: ChipId): void {
    this.chipMask |= 1 << CHIP_IDS.indexOf(id);
    if (this.cfg.onChips) return;
    const op = this.chipOps.push("", "chip");
    op.x = this.chipX;
    op.y = this.chipY;
    op.text = CHIP_TEXT[id];
    op.color = UNAVAILABLE_CHIP;
    this.chipY += 24;
  }

  private paint(now: number): void {
    const ctx = this.ctx;
    if (!ctx || this.cssW <= 0 || this.cssH <= 0) return;
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, ctx.canvas.width, ctx.canvas.height);
    ctx.setTransform(this.dpr, 0, 0, this.dpr, 0, 0);
    const cam = this.cfg.cameraId;
    const L = this.cfg.layers;
    const s = this.store;
    this.chipOps.clear();
    this.chipMask = 0;
    const main = this.videoRect(this.rectMain, 0, 0);
    // Status chips stack centred under the top chrome (a fifth down the rect).
    this.chipX = main.x + main.w / 2;
    this.chipY = main.y + main.h * 0.2;

    // Grids and zones (depth z10, surfaces z20, obstacles z30), each in its
    // own buffer, re-laid out only when its own frame changed (configure and
    // resize re-lay all three), and painted with its own staleness.
    const depthVis = visibility(s.depth.seen, s.depth.data !== null, now);
    const surfVis = visibility(s.surfaces.seen, s.surfaces.data !== null, now);
    const obstVis = visibility(s.obstacles.seen, s.obstacles.data !== null, now);
    if (this.depthLaid !== s.depth.version) {
      this.depthLaid = s.depth.version;
      this.depthOps.clear();
      const g = cam !== null && s.depth.data && this.cfg.depthRangeM > 0 ? s.depth.data.cameras[cam] : undefined;
      if (g && s.depth.data) {
        this.layoutGrid(this.depthOps, g.near, g.measured, g.cols, g.rows, {
          maxM: this.cfg.depthRangeM,
          nearM: 2,
          mode: this.cfg.depthMode,
          unscaled: (g.scaleSource ?? s.depth.data.scaleSource) === "none",
        });
      }
    }
    if (this.surfLaid !== s.surfaces.version) {
      this.surfLaid = s.surfaces.version;
      this.surfOps.clear();
      if (cam !== null && s.surfaces.data && s.surfaces.data.cameraId === cam) {
        layoutZones(s.surfaces.data.zones, this.videoRect(this.rectMain, 0, 0), this.surfOps);
      }
    }
    if (this.obstLaid !== s.obstacles.version) {
      this.obstLaid = s.obstacles.version;
      this.obstOps.clear();
      if (s.obstacles.data) {
        const g = cam !== null ? s.obstacles.data.cameras[cam] : undefined;
        if (g) this.layoutGrid(this.obstOps, g.near, g.measured, g.cols, g.rows, OBSTACLE_GRID);
        layoutSectorArc(s.obstacles.data.sectorsM, this.videoRect(this.rectMain, 0, 0), OBSTACLE_GRID, this.obstOps);
      }
    }
    this.paintLayer(ctx, L.has("depth"), depthVis, this.depthOps);
    this.paintLayer(ctx, L.has("surfaces"), surfVis, this.surfOps);
    this.paintLayer(ctx, L.has("obstacles"), obstVis, this.obstOps);
    if (L.has("depth") && depthVis === "hidden") this.chip("depth");
    else if (L.has("depth") && cam !== null && s.depth.data) {
      const g = s.depth.data.cameras[cam];
      if (g && (g.scaleSource ?? s.depth.data.scaleSource) === "none") this.chip("depthUnscaled");
    }
    if (L.has("surfaces") && surfVis === "hidden") this.chip("surfaces");
    if (L.has("obstacles") && obstVis === "hidden") this.chip("obstacles");

    // AR (route z50, motion z60).
    const wantAr = L.has("ar_route") || L.has("ar_motion");
    if (wantAr && cam !== null) {
      const arVis = visibility(s.ar.seen, s.ar.data !== null, now);
      const frame = s.ar.data;
      const arCam = frame ? (frame.cameras[cam] ?? null) : null;
      const status = arStatusChip(frame, arCam, arVis === "hidden");
      if (status) this.chip(status === "AR ≈" ? "arApprox" : "ar");
      if (arVis !== "hidden" && arCam) {
        if (this.arLaidVersion !== s.ar.version) {
          this.arLaidVersion = s.ar.version;
          const opts = {
            level: this.cfg.level,
            groups: { route: L.has("ar_route"), motion: L.has("ar_motion") },
            mission: frame?.mission ?? this.cfg.mission,
            avoid: this.cfg.avoid,
            showLoaded: this.cfg.showLoaded,
          };
          this.arNextOps.clear();
          layoutAr(arCam, this.videoRect(this.rectFrame, arCam.frameW, arCam.frameH), opts, this.arNextOps);
          this.arPrevOps.clear();
          const prevCam = s.arPrev.data?.cameras[cam];
          if (prevCam) layoutAr(prevCam, this.videoRect(this.rectFrame, prevCam.frameW, prevCam.frameH), opts, this.arPrevOps);
          const interval = s.ar.at - s.arPrev.at;
          this.interval = interval >= MIN_INTERVAL_MS && interval <= MAX_INTERVAL_MS && prevCam ? interval : 0;
          this.interpUntil = s.ar.at + Math.max(this.interval, FADE_MS);
        }
        const since = now - s.ar.at;
        const t = this.interval > 0 ? since / this.interval : 1;
        // Items new in this frame fade in over FADE_MS even after a still
        // period (no interval to interpolate over): they never pop.
        lerpOps(this.arPrevOps, this.arNextOps, t, since / FADE_MS, this.arOut);
        paintOps(ctx, this.arOut, arVis === "dim" ? 0.5 : 1);
      }
    }

    if (this.chipOps.n > 0) paintOps(ctx, this.chipOps, 1);
    this.reportChips(this.chipMask);
  }

  private interval = 0;

  private paintLayer(ctx: CanvasRenderingContext2D, on: boolean, vis: Visibility, ops: DrawOpBuffer): void {
    if (!on || vis === "hidden" || ops.n === 0) return;
    paintOps(ctx, ops, vis === "dim" ? 0.5 : 1);
  }

  private layoutGrid(
    out: DrawOpBuffer,
    near: string,
    measured: string | null,
    cols: number,
    rows: number,
    opts: { maxM: number; nearM: number; mode: "solid" | "outline"; unscaled?: boolean },
  ): void {
    const cells = cols * rows;
    if (this.nearBuf.length < cells) this.nearBuf = new Uint8Array(cells);
    const mbytes = (cells + 7) >> 3;
    if (this.measBuf.length < mbytes) this.measBuf = new Uint8Array(mbytes);
    const got = decodeBase64Into(near, this.nearBuf);
    if (got < cells) return;
    const hasMeas = measured !== null && decodeBase64Into(measured, this.measBuf) >= mbytes;
    const rect = this.videoRect(this.rectMain, 0, 0);
    layoutCells(this.nearBuf, hasMeas ? this.measBuf : null, cols, rows, rect, opts, out);
  }
}
