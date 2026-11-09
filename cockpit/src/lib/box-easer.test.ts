import { describe, expect, it } from "vitest";

import { BoxEaser, easeTransform, IDENTITY, type BoxTransform, type FrameClock } from "@/lib/box-easer";
import type { SmoothBox } from "@/shared/hud/box-smoothing";

/** A frame clock the test steps by hand, 16 ms a frame. */
function manualClock() {
  let t = 0;
  let pending: ((t: number) => void) | null = null;
  let requests = 0;
  const clock: FrameClock = {
    request: (cb) => {
      pending = cb;
      requests++;
      return requests;
    },
    cancel: () => {
      pending = null;
    },
    now: () => t,
  };
  const frame = (): boolean => {
    const cb = pending;
    pending = null;
    if (!cb) return false;
    t += 16;
    cb(t);
    return true;
  };
  return { clock, frame, requests: () => requests };
}

const box = (x: number, y: number, width = 40, height = 80): SmoothBox => ({ x, y, width, height });

/** Where a transform puts the laid-out box. */
const place = (t: BoxTransform, b: SmoothBox): SmoothBox => ({
  x: t.tx + t.sx * b.x,
  y: t.ty + t.sy * b.y,
  width: t.sx * b.width,
  height: t.sy * b.height,
});

describe("easeTransform", () => {
  it("maps the laid-out box onto the shown box", () => {
    const t = easeTransform(box(100, 50), box(90, 40, 50, 100));
    expect(place(t, box(100, 50))).toEqual(box(90, 40, 50, 100));
    expect(easeTransform(box(10, 10), box(10, 10))).toEqual(IDENTITY);
  });
});

describe("BoxEaser", () => {
  it("eases a moved box from where it was shown to its new box, then stops", () => {
    const c = manualClock();
    const applied: BoxTransform[] = [];
    const easer = new BoxEaser((_, t) => applied.push(t), 120, 0.5, c.clock);

    easer.setTargets(new Map([["a", box(100, 100)]]));
    expect(applied.at(-1)).toEqual(IDENTITY); // a new box appears in place
    while (c.frame()) {
      /* settle */
    }

    applied.length = 0;
    const next = box(200, 100);
    easer.setTargets(new Map([["a", next]]));
    // Drawn where it was before the browser paints the new layout.
    expect(place(applied[0], next).x).toBeCloseTo(100, 6);
    let frames = 0;
    while (c.frame()) frames++;
    const xs = applied.map((t) => place(t, next).x);
    for (let i = 1; i < xs.length; i++) expect(xs[i]).toBeGreaterThanOrEqual(xs[i - 1]);
    expect(applied.at(-1)).toEqual(IDENTITY);
    expect(frames).toBeGreaterThan(5);
    expect(c.frame()).toBe(false); // the loop idles once converged
  });

  it("snaps under reduced motion without running a loop", () => {
    const c = manualClock();
    const applied: BoxTransform[] = [];
    const easer = new BoxEaser((_, t) => applied.push(t), 120, 0.5, c.clock);
    easer.setTargets(new Map([["a", box(0, 0)]]), true);
    easer.setTargets(new Map([["a", box(300, 0)]]), true);
    expect(applied).toEqual([IDENTITY, IDENTITY]);
    expect(c.requests()).toBe(0);
  });
});
