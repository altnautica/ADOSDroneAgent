import { describe, expect, it } from "vitest";

import { activeDisplayId, displayRows } from "@/lib/settings/display";

describe("display options", () => {
  const res = {
    board_id: "rpi4",
    current: { display_id: "hdmi" },
    supported: [
      { id: "hdmi", label: "HDMI", resolution: "1920x1080" },
      { id: "spi35", label: "3.5in SPI", touch_chip: "xpt2046" },
    ],
  };

  it("marks the active overlay and carries resolution and touch chip", () => {
    expect(displayRows(res)).toEqual([
      { id: "hdmi", label: "HDMI", resolution: "1920x1080", touchChip: null, active: true },
      { id: "spi35", label: "3.5in SPI", resolution: null, touchChip: "xpt2046", active: false },
    ]);
  });

  it("has no active overlay when none is reported, and no rows without data", () => {
    expect(activeDisplayId({ ...res, current: {} })).toBeNull();
    expect(displayRows({ ...res, current: undefined }).some((r) => r.active)).toBe(false);
    expect(displayRows(null)).toEqual([]);
  });
});
