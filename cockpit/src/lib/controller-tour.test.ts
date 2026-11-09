import { describe, expect, it } from "vitest";

import { tourStage } from "@/lib/controller-tour";

describe("first-connect controller tour", () => {
  it("opens on the first controller anywhere, once", () => {
    expect(tourStage(true, true, false)).toBe("tour");
    expect(tourStage(true, true, true)).toBe("none");
  });

  it("asks for a button press while the controller is announced but not reported", () => {
    expect(tourStage(false, true, false)).toBe("press");
    expect(tourStage(false, false, false)).toBe("none");
  });
});
