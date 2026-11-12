/**
 * @module hud/layer-frame-store
 * @description The plain mutable store behind a video's data layers: the
 * latest frame of each layer kind and the previous `ar` frame, written by the
 * stream or poll and read by the layer engine (never React state per frame).
 *
 * A keep-alive (the same `ts_ms` again, sent by the agent about once a second
 * while a scene is unchanged) refreshes only the receipt time: it neither
 * restarts interpolation nor replaces the previous frame.
 *
 * Dependency-free apart from its siblings: Mission Control copies it.
 *
 * @license GPL-3.0-only
 */

import type { ArFrame, DepthFrame, ObstaclesFrame, SurfacesFrame } from "./cockpit-frames";

interface Slot<T> {
  data: T | null;
  /** When this frame (a new `ts_ms`) arrived: the interpolation clock. */
  at: number;
  /** When this frame or its keep-alive last arrived: the staleness clock. */
  seen: number;
  version: number;
}

function slot<T>(): Slot<T> {
  return { data: null, at: 0, seen: 0, version: 0 };
}

/** A repeat of the held frame: the agent's keep-alive. */
function isKeepAlive<T extends { tsMs: number }>(s: Slot<T>, f: T): boolean {
  return s.data !== null && f.tsMs !== 0 && f.tsMs === s.data.tsMs;
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
    if (this.keepAlive(this.ar, f, at)) return;
    this.arPrev.data = this.ar.data;
    this.arPrev.at = this.ar.at;
    this.arPrev.seen = this.ar.seen;
    this.put(this.ar, f, at);
  }
  putObstacles(f: ObstaclesFrame, at = performance.now()): void {
    if (!this.keepAlive(this.obstacles, f, at)) this.put(this.obstacles, f, at);
  }
  putDepth(f: DepthFrame, at = performance.now()): void {
    if (!this.keepAlive(this.depth, f, at)) this.put(this.depth, f, at);
  }
  putSurfaces(f: SurfacesFrame, at = performance.now()): void {
    if (!this.keepAlive(this.surfaces, f, at)) this.put(this.surfaces, f, at);
  }
  private put<T>(s: Slot<T>, f: T, at: number): void {
    s.data = f;
    s.at = at;
    s.seen = at;
    s.version++;
    this.version++;
  }
  private keepAlive<T extends { tsMs: number }>(s: Slot<T>, f: T, at: number): boolean {
    if (!isKeepAlive(s, f)) return false;
    // Only the staleness clock moves; the loop wakes in case a dimmed layer
    // turns fresh again.
    if (at > s.seen) s.seen = at;
    this.version++;
    return true;
  }
  clear(): void {
    for (const s of [this.ar, this.arPrev, this.obstacles, this.depth, this.surfaces]) {
      s.data = null;
      s.at = 0;
      s.seen = 0;
      s.version++;
    }
    this.version++;
  }
}
