import { describe, expect, it } from "vitest";

import {
  cycleFocus,
  dragAccepted,
  dragFrameBox,
  dragScreenRect,
  dragStarted,
  focusOrder,
} from "./target-interaction";
import type { GlyphLayout } from "./target-layout";

describe("drag to designate", () => {
  it("normalises the rect whichever way it was dragged", () => {
    expect(dragScreenRect({ x: 50, y: 40 }, { x: 10, y: 100 })).toEqual({ x: 10, y: 40, w: 40, h: 60 });
  });

  it("starts a drag at 12 px of travel", () => {
    expect(dragStarted({ x: 0, y: 0 }, { x: 11, y: 0 })).toBe(false);
    expect(dragStarted({ x: 0, y: 0 }, { x: 12, y: 0 })).toBe(true);
  });

  it("maps the screen rect into frame px through the letterboxed rect, clipped", () => {
    // 1280×720 frame drawn at half size, 40 px letterbox top.
    const rr = { left: 0, top: 40, width: 640, height: 360 };
    expect(dragFrameBox({ x: 100, y: 90, w: 50, h: 100 }, rr, 1280, 720)).toEqual({ x: 200, y: 100, width: 100, height: 200 });
    // A drag into the letterbox bar is clipped to the frame.
    expect(dragFrameBox({ x: 600, y: 0, w: 100, h: 60 }, rr, 1280, 720)).toEqual({ x: 1200, y: 0, width: 80, height: 40 });
  });

  it("ignores a rect under 1 % of the frame", () => {
    expect(dragAccepted({ x: 0, y: 0, width: 12, height: 100 }, 1280, 720)).toBe(false);
    expect(dragAccepted({ x: 0, y: 0, width: 13, height: 8 }, 1280, 720)).toBe(true);
  });
});

describe("focus cycle", () => {
  const glyph = (key: string, x: number, hit = true) =>
    ({ key, box: { x, y: 0, w: 10, h: 10 }, hit: hit ? { x, y: 0, w: 44, h: 44 } : null }) as unknown as GlyphLayout;

  it("orders hittable boxes left to right", () => {
    expect(focusOrder([glyph("b", 300), glyph("a", 20), glyph("ghost", 5, false)])).toEqual(["a", "b"]);
  });

  it("cycles with wrap-around and enters from either end", () => {
    const order = ["a", "b", "c"];
    expect(cycleFocus(order, null, 1)).toBe("a");
    expect(cycleFocus(order, null, -1)).toBe("c");
    expect(cycleFocus(order, "c", 1)).toBe("a");
    expect(cycleFocus(order, "a", -1)).toBe("c");
    expect(cycleFocus(order, "gone", 1)).toBe("a");
    expect(cycleFocus([], "a", 1)).toBeNull();
  });
});
