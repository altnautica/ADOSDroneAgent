// The rail's scroll affordance: when its entries do not fit (ten
// ground-station tabs on a 1280×800 kiosk), the clipped end says so.

import { describe, expect, it } from "vitest";

import { railOverflow } from "./rail-overflow";

const box = { scrollTop: 0, scrollLeft: 0, scrollHeight: 400, scrollWidth: 80, clientHeight: 400, clientWidth: 80 };

describe("the rail's overflow", () => {
  it("is none when every entry fits", () => {
    expect(railOverflow(box)).toEqual({ axis: null, before: false, after: false });
  });

  it("marks more below at the top of a tall rail, both ends midway, more above at the bottom", () => {
    const tall = { ...box, scrollHeight: 700 };
    expect(railOverflow(tall)).toEqual({ axis: "y", before: false, after: true });
    expect(railOverflow({ ...tall, scrollTop: 150 })).toEqual({ axis: "y", before: true, after: true });
    expect(railOverflow({ ...tall, scrollTop: 300 })).toEqual({ axis: "y", before: true, after: false });
  });

  it("follows the portrait strip's horizontal scroll", () => {
    const wide = { ...box, scrollHeight: 60, clientHeight: 60, scrollWidth: 900, clientWidth: 400 };
    expect(railOverflow(wide)).toEqual({ axis: "x", before: false, after: true });
    expect(railOverflow({ ...wide, scrollLeft: 500 })).toEqual({ axis: "x", before: true, after: false });
  });
});
