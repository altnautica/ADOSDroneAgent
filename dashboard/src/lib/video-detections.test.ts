import { describe, expect, it } from "vitest";

import { layoutVideoBoxes, parseLatestDetections } from "@/lib/video-detections";

const reply = (extra: Record<string, unknown> = {}) => ({
  available: true,
  camera_id: "main",
  frame_width: 640,
  frame_height: 360,
  detections: [
    { bbox: { x: 320, y: 90, width: 64, height: 180 }, class_label: "person", confidence: 0.9, track_id: 12, lock_state: "locked" },
    { bbox: { x: 10, y: 10, width: 0, height: 5 }, class_label: "car", confidence: 0.5 },
    { class_label: "mask-only" },
  ],
  ...extra,
});

describe("parseLatestDetections", () => {
  it("keeps the boxed detections of the asked camera", () => {
    const f = parseLatestDetections(reply(), "main")!;
    expect(f.frameW).toBe(640);
    expect(f.boxes).toEqual([{ x: 320, y: 90, w: 64, h: 180, label: "person 90%", trackId: 12, locked: true }]);
  });

  it("draws nothing for vision off, a stale batch or another camera", () => {
    expect(parseLatestDetections({ detections: [], available: false }, "main")).toBeNull();
    expect(parseLatestDetections({ detections: [], stale: true, available: true }, "main")).toBeNull();
    expect(parseLatestDetections(reply({ camera_id: "down" }), "main")).toBeNull();
  });
});

describe("layoutVideoBoxes", () => {
  it("places a box inside the letterboxed video, not the panel", () => {
    const f = parseLatestDetections(reply(), "main")!;
    // A 16:9 video in a 400x400 panel: 225 px tall, 87.5 px bars top and bottom.
    const [b] = layoutVideoBoxes(f, 400, 400, 1280, 720);
    expect(b.x).toBeCloseTo(200, 6);
    expect(b.y).toBeCloseTo(87.5 + 90 * (225 / 360), 6);
    expect(b.label).toBe("person 90% #12");
    expect(b.path.startsWith("M")).toBe(true);
    expect(b.locked).toBe(true);
  });
});
