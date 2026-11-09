import { describe, expect, it } from "vitest";

import { CHIP_TEXT, UNAVAILABLE_CHIP } from "./layer-engine";
import { DrawOpBuffer } from "./layer-layout";
import { paintOps } from "./layer-paint";
import tokens from "../cockpit-spec/tokens.json";

function recordingCtx() {
  const dashes: number[][] = [];
  const noop = () => undefined;
  const ctx = {
    beginPath: noop,
    moveTo: noop,
    lineTo: noop,
    closePath: noop,
    rect: noop,
    arc: noop,
    roundRect: noop,
    fill: noop,
    stroke: noop,
    strokeText: noop,
    fillText: noop,
    measureText: (t: string) => ({ width: t.length * 7 }),
    setLineDash: (d: number[]) => {
      dashes.push(d);
    },
  };
  return { ctx: ctx as unknown as CanvasRenderingContext2D, dashes };
}

describe("paintOps", () => {
  it("passes the same dash arrays on every paint (no per-op allocation)", () => {
    const buf = new DrawOpBuffer();
    const dashed = buf.push("a", "line");
    dashed.pts = [0, 0, 10, 10];
    dashed.dash = 6;
    const solid = buf.push("b", "line");
    solid.pts = [0, 0, 5, 5];
    const { ctx, dashes } = recordingCtx();
    paintOps(ctx, buf);
    const first = dashes.slice();
    dashes.length = 0;
    paintOps(ctx, buf);
    expect(dashes).toHaveLength(first.length);
    dashes.forEach((d, i) => expect(d).toBe(first[i]));
    expect(first[0]).toEqual([6, 6 * 0.8]);
  });
});

describe("layer status chips", () => {
  it("take their colour from the tokens and their text from the spec strings", () => {
    expect(UNAVAILABLE_CHIP).toBe(tokens.layer.unavailable_chip);
    expect(CHIP_TEXT.depth).toBe("Depth unavailable");
    expect(CHIP_TEXT.depthUnscaled).toBe("Depth unscaled");
    expect(CHIP_TEXT.ar).toBe("AR unavailable");
  });
});
