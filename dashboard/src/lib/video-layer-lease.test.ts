import { describe, expect, it } from "vitest";

import { cameraOptions, DEFAULT_CAMERA_ID, holdLayerDemand, layersViewQuery, type VisionDemandBody } from "@/lib/video-layer-lease";

describe("camera choice for detections and layers", () => {
  it("defaults to the primary camera and lists the roster legs after it", () => {
    expect(DEFAULT_CAMERA_ID).toBe("main");
    expect(cameraOptions(undefined)).toEqual(["main"]);
    expect(cameraOptions({ cameras: [{ id: "thermal" }, { id: "main" }, { id: "" }, { id: 3 }, { id: "down" }] })).toEqual([
      "main",
      "thermal",
      "down",
    ]);
  });

  it("leases the view and the demand on the chosen camera", () => {
    expect(layersViewQuery({}, "thermal")).toBe("thermal:ar_route,ar_motion,obstacles");
    const sent: VisionDemandBody[] = [];
    const doc = { visibilityState: "visible" as const, addEventListener: () => undefined, removeEventListener: () => undefined };
    const release = holdLayerDemand("depth", (b) => sent.push(b), doc, "thermal");
    release();
    expect(sent.map((b) => b.camera_id)).toEqual(["thermal", "thermal"]);
  });
});
