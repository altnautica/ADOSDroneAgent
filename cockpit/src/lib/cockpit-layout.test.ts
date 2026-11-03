import { describe, expect, it } from "vitest";

import {
  clampSplit,
  feedLayoutFor,
  skillSlotPx,
  SPLIT_DEFAULT,
  widthClassFor,
} from "@/lib/cockpit-layout";

describe("width classes", () => {
  it("splits at 600, 1024 and 1600 px", () => {
    expect(widthClassFor(599, false)).toBe("compact");
    expect(widthClassFor(600, false)).toBe("medium");
    expect(widthClassFor(1023, false)).toBe("medium");
    expect(widthClassFor(1024, false)).toBe("expanded");
    expect(widthClassFor(1599, false)).toBe("expanded");
    expect(widthClassFor(1600, false)).toBe("tv");
  });

  it("treats the kiosk as a tv at any width", () => {
    expect(widthClassFor(800, true)).toBe("tv");
  });
});

describe("feed layouts", () => {
  it("stacks in portrait whatever the width", () => {
    expect(feedLayoutFor("tv", true, true)).toBe("portrait");
    expect(feedLayoutFor("compact", true, false)).toBe("portrait");
  });

  it("keeps the full-bleed video on a compact window", () => {
    expect(feedLayoutFor("compact", false, true)).toBe("compact");
  });

  it("splits video and map from medium up", () => {
    expect(feedLayoutFor("medium", false, true)).toBe("split");
    expect(feedLayoutFor("expanded", false, true)).toBe("split");
    expect(feedLayoutFor("tv", false, true)).toBe("split");
  });

  it("never splits off an empty map pane", () => {
    expect(feedLayoutFor("expanded", false, false)).toBe("compact");
  });
});

describe("layout measures", () => {
  it("keeps the split between 0.2 and 0.8", () => {
    expect(clampSplit(0.05)).toBe(0.2);
    expect(clampSplit(0.95)).toBe(0.8);
    expect(clampSplit(0.5)).toBe(0.5);
    expect(clampSplit(Number.NaN)).toBe(SPLIT_DEFAULT);
  });

  it("sizes Skill Bar slots by width class and glove mode", () => {
    expect(skillSlotPx("compact", false)).toBe(64);
    expect(skillSlotPx("medium", false)).toBe(72);
    expect(skillSlotPx("expanded", false)).toBe(88);
    expect(skillSlotPx("tv", false)).toBe(88);
    expect(skillSlotPx("medium", true)).toBe(90);
  });
});
