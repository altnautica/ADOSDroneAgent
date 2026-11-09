// Detection box smoothing outside React: the glyphs are laid out on each
// batch's own boxes, and this eases a per-box transform from where the box was
// drawn toward identity, frame by frame, written straight onto the glyph's
// elements. A moving box therefore costs no React render per frame; the loop
// runs only until every box has converged.

import { advanceBoxes, smoothingAlpha, type SmoothBox } from "@/shared/hud/box-smoothing";

/** The 2D map from a box's laid-out rect onto its eased rect. */
export interface BoxTransform {
  tx: number;
  ty: number;
  sx: number;
  sy: number;
}

/** The transform that draws a glyph laid out on `target` at `shown`. Identity
 *  when the two match. */
export function easeTransform(target: SmoothBox, shown: SmoothBox): BoxTransform {
  const sx = target.width > 0 ? shown.width / target.width : 1;
  const sy = target.height > 0 ? shown.height / target.height : 1;
  return { tx: shown.x - target.x * sx, ty: shown.y - target.y * sy, sx, sy };
}

export const IDENTITY: BoxTransform = { tx: 0, ty: 0, sx: 1, sy: 1 };

export interface FrameClock {
  request(cb: (t: number) => void): number;
  cancel(id: number): void;
  now(): number;
}

const browserClock: FrameClock = {
  request: (cb) => requestAnimationFrame(cb),
  cancel: (id) => cancelAnimationFrame(id),
  now: () => performance.now(),
};

/** Eases the displayed boxes toward each new set of laid-out boxes and calls
 *  `apply(key, transform)` for every box that moved this frame. */
export class BoxEaser {
  private shown = new Map<string, SmoothBox>();
  private targets: ReadonlyMap<string, SmoothBox> = new Map();
  private raf = 0;
  private last = 0;

  constructor(
    private readonly apply: (key: string, t: BoxTransform) => void,
    private readonly tauMs: number,
    private readonly epsPx: number,
    private readonly clock: FrameClock = browserClock,
  ) {}

  /** New laid-out boxes (screen px). A new key appears in place; a known key
   *  starts from where it was shown. With `snap` every box jumps to its target. */
  setTargets(targets: ReadonlyMap<string, SmoothBox>, snap = false): void {
    this.targets = targets;
    if (snap) {
      this.stop();
      this.shown = new Map(targets);
      for (const key of targets.keys()) this.apply(key, IDENTITY);
      return;
    }
    // Draw this frame's glyphs where the boxes were, before the browser paints.
    for (const [key, t] of targets) {
      const s = this.shown.get(key);
      this.apply(key, s ? easeTransform(t, s) : IDENTITY);
    }
    if (this.raf === 0) {
      this.last = this.clock.now();
      this.raf = this.clock.request(this.step);
    }
  }

  stop(): void {
    if (this.raf !== 0) this.clock.cancel(this.raf);
    this.raf = 0;
  }

  private step = (t: number): void => {
    this.raf = 0;
    const r = advanceBoxes(this.shown, this.targets, smoothingAlpha(t - this.last, this.tauMs), this.epsPx);
    this.last = t;
    if (r.next !== this.shown) {
      this.shown = r.next;
      for (const [key, target] of this.targets) {
        const s = this.shown.get(key);
        if (s) this.apply(key, easeTransform(target, s));
      }
    }
    if (!r.settled) this.raf = this.clock.request(this.step);
  };
}
