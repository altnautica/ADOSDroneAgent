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
 * Staleness (cockpit-spec layers.json): a layer older than 500 ms dims, older
 * than 2 s it hides and an "<layer> unavailable" chip shows.
 *
 * `fpsCap` limits paints (30 on the kiosk / TV class). `stats()` reports the
 * paint durations for the overlay budget checks.
 *
 * Dependency-free apart from its siblings: Mission Control copies it.
 *
 * @license GPL-3.0-only
 */

import { arStatusChip, layoutAr, type LayerLevel } from "./ar-layout";
import {
  decodeBase64Into,
  type ArFrame,
  type AvoidState,
  type DepthFrame,
  type MissionState,
  type ObstaclesFrame,
  type SurfacesFrame,
} from "./cockpit-frames";
import {
  DrawOpBuffer,
  containRect,
  layoutCells,
  layoutSectorArc,
  layoutZones,
  lerpOps,
  paintOps,
  type Rect,
} from "./layer-layout";

export const DIM_AFTER_MS = 500;
export const HIDE_AFTER_MS = 2000;
/** Interpolation interval bounds (ms): a gap outside snaps. */
const MIN_INTERVAL_MS = 30;
const MAX_INTERVAL_MS = 250;
const FADE_MS = 120;

interface Slot<T> {
  data: T | null;
  at: number;
  version: number;
}

function slot<T>(): Slot<T> {
  return { data: null, at: 0, version: 0 };
}

/**
 * The latest frame of each layer kind and the previous `ar` frame, written by
 * the stream or poll and read by the engine. Writes bump `version`, which is
 * what wakes the loop.
 */
export class LayerFrameStore {
  ar = slot<ArFrame>();
  arPrev = slot<ArFrame>();
  obstacles = slot<ObstaclesFrame>();
  depth = slot<DepthFrame>();
  surfaces = slot<SurfacesFrame>();
  version = 0;

  putAr(f: ArFrame, at = performance.now()): void {
    this.arPrev.data = this.ar.data;
    this.arPrev.at = this.ar.at;
    this.ar.data = f;
    this.ar.at = at;
    this.ar.version++;
    this.version++;
  }
  putObstacles(f: ObstaclesFrame, at = performance.now()): void {
    this.obstacles.data = f;
    this.obstacles.at = at;
    this.obstacles.version++;
    this.version++;
  }
  putDepth(f: DepthFrame, at = performance.now()): void {
    this.depth.data = f;
    this.depth.at = at;
    this.depth.version++;
    this.version++;
  }
  putSurfaces(f: SurfacesFrame, at = performance.now()): void {
    this.surfaces.data = f;
    this.surfaces.at = at;
    this.surfaces.version++;
    this.version++;
  }
  clear(): void {
    for (const s of [this.ar, this.arPrev, this.obstacles, this.depth, this.surfaces]) {
      s.data = null;
      s.at = 0;
      s.version++;
    }
    this.version++;
  }
}

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
  private lastVis = "";
  private arLaidVersion = -1;
  private cellLaidKey = "";
  private interpUntil = 0;

  private arPrevOps = new DrawOpBuffer();
  private arNextOps = new DrawOpBuffer();
  private arOut = new DrawOpBuffer();
  private cellOps = new DrawOpBuffer();
  private chipOps = new DrawOpBuffer();
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
    this.cellLaidKey = "";
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
    this.cellLaidKey = "";
    this.invalidate();
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

  /** A string that changes exactly when some layer crosses a staleness step. */
  private visKey(now: number): string {
    const s = this.store;
    return (
      visibility(s.ar.at, s.ar.data !== null, now)[0] +
      visibility(s.obstacles.at, s.obstacles.data !== null, now)[0] +
      visibility(s.depth.at, s.depth.data !== null, now)[0] +
      visibility(s.surfaces.at, s.surfaces.data !== null, now)[0]
    );
  }

  private videoRect(fw: number, fh: number): Rect {
    return containRect(this.cssW, this.cssH, fw || this.cfg.videoW, fh || this.cfg.videoH);
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
    const chipRect = this.videoRect(0, 0);
    // Status chips stack centred under the top chrome (a fifth down the rect).
    let chipY = chipRect.y + chipRect.h * 0.2;
    const chip = (text: string) => {
      const op = this.chipOps.push("", "chip");
      op.x = chipRect.x + chipRect.w / 2;
      op.y = chipY;
      op.text = text;
      op.color = "#909090";
      chipY += 24;
    };

    // Grids and zones (depth z10, surfaces z20, obstacles z30) — re-laid out
    // only when their frame, the camera, the size or the config changed.
    const cellKey = `${s.depth.version}/${s.surfaces.version}/${s.obstacles.version}/${cam}`;
    const depthVis = visibility(s.depth.at, s.depth.data !== null, now);
    const surfVis = visibility(s.surfaces.at, s.surfaces.data !== null, now);
    const obstVis = visibility(s.obstacles.at, s.obstacles.data !== null, now);
    if (cellKey !== this.cellLaidKey) {
      this.cellLaidKey = cellKey;
      this.cellOps.clear();
      if (cam !== null && L.has("depth") && s.depth.data && this.cfg.depthRangeM > 0) {
        const g = s.depth.data.cameras[cam];
        if (g) {
          this.layoutGrid(g.near, g.measured, g.cols, g.rows, {
            maxM: this.cfg.depthRangeM,
            nearM: 2,
            mode: this.cfg.depthMode,
            unscaled: (g.scaleSource ?? s.depth.data.scaleSource) === "none",
          });
        }
      }
      if (cam !== null && L.has("surfaces") && s.surfaces.data && s.surfaces.data.cameraId === cam) {
        layoutZones(s.surfaces.data.zones, this.videoRect(0, 0), this.cellOps);
      }
      if (L.has("obstacles") && s.obstacles.data) {
        const g = cam !== null ? s.obstacles.data.cameras[cam] : undefined;
        if (g) this.layoutGrid(g.near, g.measured, g.cols, g.rows, { maxM: 5, nearM: 2, mode: "solid" });
        layoutSectorArc(s.obstacles.data.sectorsM, this.videoRect(0, 0), { maxM: 5, nearM: 2 }, this.cellOps);
      }
    }
    if (L.has("depth") && depthVis === "hidden") chip("Depth unavailable");
    else if (L.has("depth") && cam !== null && s.depth.data) {
      const g = s.depth.data.cameras[cam];
      if (g && (g.scaleSource ?? s.depth.data.scaleSource) === "none") chip("Depth unscaled");
    }
    if (L.has("surfaces") && surfVis === "hidden") chip("Surfaces unavailable");
    if (L.has("obstacles") && obstVis === "hidden") chip("Obstacles unavailable");
    // One staleness alpha for the cell group: the freshest visible source.
    const cellHidden =
      (depthVis === "hidden" || !L.has("depth")) &&
      (surfVis === "hidden" || !L.has("surfaces")) &&
      (obstVis === "hidden" || !L.has("obstacles"));
    if (!cellHidden && this.cellOps.n > 0) {
      const dim = depthVis !== "fresh" && surfVis !== "fresh" && obstVis !== "fresh";
      paintOps(ctx, this.cellOps, dim ? 0.5 : 1);
    }

    // AR (route z50, motion z60).
    const wantAr = L.has("ar_route") || L.has("ar_motion");
    if (wantAr && cam !== null) {
      const arVis = visibility(s.ar.at, s.ar.data !== null, now);
      const frame = s.ar.data;
      const arCam = frame ? (frame.cameras[cam] ?? null) : null;
      const status = arStatusChip(frame, arCam, arVis === "hidden");
      if (status) chip(status);
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
          layoutAr(arCam, this.videoRect(arCam.frameW, arCam.frameH), opts, this.arNextOps);
          this.arPrevOps.clear();
          const prevCam = s.arPrev.data?.cameras[cam];
          if (prevCam) layoutAr(prevCam, this.videoRect(prevCam.frameW, prevCam.frameH), opts, this.arPrevOps);
          const interval = s.ar.at - s.arPrev.at;
          this.interval = interval >= MIN_INTERVAL_MS && interval <= MAX_INTERVAL_MS && prevCam ? interval : 0;
          this.interpUntil = s.ar.at + Math.max(this.interval, FADE_MS);
        }
        const since = now - s.ar.at;
        const t = this.interval > 0 ? since / this.interval : 1;
        lerpOps(this.arPrevOps, this.arNextOps, t, this.interval > 0 ? since / FADE_MS : 1, this.arOut);
        paintOps(ctx, this.arOut, arVis === "dim" ? 0.5 : 1);
      }
    }

    if (this.chipOps.n > 0) paintOps(ctx, this.chipOps, 1);
  }

  private interval = 0;

  private layoutGrid(
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
    const rect = this.videoRect(0, 0);
    layoutCells(this.nearBuf, hasMeas ? this.measBuf : null, cols, rows, rect, opts, this.cellOps);
  }
}
