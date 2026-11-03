import { describe, expect, it } from "vitest";

import { MENU_MARGIN_PX, placeMenu } from "@/lib/menu-placement";

describe("menu placement", () => {
  it("opens under the anchor when there is room", () => {
    const p = placeMenu({ left: 100, top: 40, bottom: 80 }, 1000, 600);
    expect(p.top).toBe(80);
    expect(p.bottom).toBeUndefined();
    expect(p.maxHeight).toBe(600 - 80 - MENU_MARGIN_PX);
  });

  it("flips above the anchor near the bottom edge", () => {
    const p = placeMenu({ left: 100, top: 500, bottom: 540 }, 1000, 600);
    expect(p.top).toBeUndefined();
    expect(p.bottom).toBe(100);
    expect(p.maxHeight).toBe(500 - MENU_MARGIN_PX);
  });

  it("keeps the menu inside the window horizontally", () => {
    expect(placeMenu({ left: 990, top: 40, bottom: 80 }, 1000, 600).left).toBe(1000 - 200 - MENU_MARGIN_PX);
    expect(placeMenu({ left: -20, top: 40, bottom: 80 }, 1000, 600).left).toBe(MENU_MARGIN_PX);
  });

  it("opens at the corner without an anchor", () => {
    expect(placeMenu(null, 1000, 600)).toEqual({ left: MENU_MARGIN_PX, top: MENU_MARGIN_PX, maxHeight: 584 });
  });
});
