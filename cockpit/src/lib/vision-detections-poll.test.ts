import { afterEach, describe, expect, it, vi } from "vitest";

const replies: unknown[] = [];
vi.mock("@/shared/api-fetch", () => ({ apiFetch: vi.fn(async () => replies.shift() ?? null) }));

import { connectGroundDetectionPoll } from "@/lib/vision-detections-poll";
import { useDetectionsStore } from "@/stores/detections-store";

const det = (x: number, track_id: number) => ({
  bbox: { x, y: 100, width: 40, height: 100 },
  class_label: "person",
  confidence: 0.9,
  track_id,
});

afterEach(() => {
  replies.length = 0;
  useDetectionsStore.getState().clear();
});

describe("ground detection poll", () => {
  it("holds the whole-frame boxes a crop-only reply carries, though the poll never saw that batch", async () => {
    replies.push({
      v: 4,
      camera_id: "main",
      frame_id: 2,
      ts_ms: 1100,
      detections: [det(290, 2)],
      crop_window: { x: 200, y: 50, width: 200, height: 200 },
      full_frame: { v: 4, camera_id: "main", frame_id: 1, ts_ms: 1000, detections: [det(20, 1), det(280, 2)] },
    });
    const stop = connectGroundDetectionPoll({ peer: "drone" });
    await vi.waitFor(() => expect(useDetectionsStore.getState().latest?.frameId).toBe(2));
    // The crop's own box plus the whole frame's box outside the window.
    expect(useDetectionsStore.getState().latest!.detections.map((d) => d.trackId)).toEqual([2, 1]);
    stop();
  });
});
