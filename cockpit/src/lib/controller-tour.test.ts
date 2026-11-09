import { describe, expect, it } from "vitest";

import { tourDeferred, tourStage } from "@/lib/controller-tour";

describe("first-connect controller tour", () => {
  it("opens on the first controller, once", () => {
    expect(tourStage(true, true, false, false)).toBe("tour");
    expect(tourStage(true, true, true, false)).toBe("none");
  });

  it("asks for a button press while the controller is announced but not reported", () => {
    expect(tourStage(false, true, false, false)).toBe("press");
    expect(tourStage(false, false, false, false)).toBe("none");
  });

  it("never opens over the flight screen while armed or flying", () => {
    const flying = tourDeferred(true, { armed: true, landed: false }, null);
    expect(flying).toBe(true);
    expect(tourStage(true, true, false, flying)).toBe("none");
    expect(tourStage(false, true, false, flying)).toBe("none");
    // Armed on the ground, or disarmed but still reported in the air.
    expect(tourDeferred(true, { armed: true, landed: true }, null)).toBe(true);
    expect(tourDeferred(true, { armed: false, landed: false }, null)).toBe(true);
    // No pilot state: the telemetry's arm state decides.
    expect(tourDeferred(true, null, true)).toBe(true);
  });

  it("opens once on the ground, or away from the flight screen", () => {
    expect(tourDeferred(true, { armed: false, landed: true }, true)).toBe(false);
    expect(tourDeferred(true, null, false)).toBe(false);
    expect(tourDeferred(false, { armed: true, landed: false }, true)).toBe(false);
  });
});
