// @vitest-environment happy-dom
// A flight-screen tip stays clear of the Vision view's side panel: its engine
// card is the first thing the operator reads there.

import { afterEach, describe, expect, it } from "vitest";

import { tipAvoidRects, VISION_PANEL_SELECTOR } from "./tip-rects";

afterEach(() => {
  document.body.innerHTML = "";
});

describe("the rects a tip avoids", () => {
  it("include the Vision side panel while it is open", () => {
    const mark = { x: 1500, y: 60, w: 420, h: 1000 };
    const measure = (el: Element) => (el.matches(VISION_PANEL_SELECTOR) ? mark : null);
    expect(tipAvoidRects(measure)).not.toContainEqual(mark);
    const panel = document.createElement("aside");
    panel.setAttribute("data-testid", "vision-panel");
    document.body.appendChild(panel);
    expect(tipAvoidRects(measure)).toContainEqual(mark);
  });
});
