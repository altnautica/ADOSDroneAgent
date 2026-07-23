import { describe, expect, it } from "vitest";

import type { CockpitDetectionBatch } from "@/stores/detections-store";
import {
  computeRenderedRect,
  pickActiveBatch,
  scaleBoxToRect,
} from "@/lib/overlay-geometry";

function batch(cameraId: string, receivedAt: number): CockpitDetectionBatch {
  return {
    modelId: "m",
    cameraId,
    frameId: 0,
    tsMs: 0,
    frameWidth: 640,
    frameHeight: 480,
    detections: [],
    receivedAt,
  };
}

describe("computeRenderedRect", () => {
  it("fills a container whose AR matches the stream (no letterbox)", () => {
    // 640x480 stream in a 1280x960 container (same 4:3 AR) -> fills it.
    const r = computeRenderedRect(1280, 960, 640, 480);
    expect(r).toEqual({ left: 0, top: 0, width: 1280, height: 960 });
  });

  it("letterboxes horizontally when the container is wider than the stream", () => {
    // 4:3 stream in a 16:9 container -> pillarbox: bars on the sides.
    const r = computeRenderedRect(1600, 900, 640, 480);
    // scale = min(1600/640=2.5, 900/480=1.875) = 1.875
    expect(r.height).toBeCloseTo(900, 5);
    expect(r.width).toBeCloseTo(640 * 1.875, 5); // 1200
    expect(r.left).toBeCloseTo((1600 - 1200) / 2, 5); // 200
    expect(r.top).toBeCloseTo(0, 5);
  });

  it("letterboxes vertically when the container is taller than the stream", () => {
    // 16:9 stream in a 4:3 container -> bars on top/bottom.
    const r = computeRenderedRect(1200, 1200, 1920, 1080);
    // scale = min(1200/1920=0.625, 1200/1080=1.111) = 0.625
    expect(r.width).toBeCloseTo(1200, 5);
    expect(r.height).toBeCloseTo(1080 * 0.625, 5); // 675
    expect(r.top).toBeCloseTo((1200 - 675) / 2, 5); // 262.5
    expect(r.left).toBeCloseTo(0, 5);
  });

  it("degrades to the full container when the stream size is unknown", () => {
    expect(computeRenderedRect(800, 480, 0, 0)).toEqual({
      left: 0,
      top: 0,
      width: 800,
      height: 480,
    });
  });
});

describe("scaleBoxToRect", () => {
  const rect = { left: 200, top: 0, width: 1200, height: 900 };

  it("maps a frame-pixel box to its fraction of the rendered rect", () => {
    // A box at the frame centre (320,240) 64x48 in a 640x480 frame.
    const placed = scaleBoxToRect(
      { x: 320, y: 240, width: 64, height: 48 },
      640,
      480,
      rect,
    );
    expect(placed).not.toBeNull();
    // 320/640 = 0.5 of width -> 200 + 0.5*1200 = 800
    expect(placed!.left).toBeCloseTo(800, 5);
    // 240/480 = 0.5 of height -> 0 + 0.5*900 = 450
    expect(placed!.top).toBeCloseTo(450, 5);
    expect(placed!.width).toBeCloseTo((64 / 640) * 1200, 5); // 120
    expect(placed!.height).toBeCloseTo((48 / 480) * 900, 5); // 90
  });

  it("clamps a box that overruns the frame edge inside the rendered rect", () => {
    // A box near the right/bottom edge that would spill past the rect.
    const placed = scaleBoxToRect(
      { x: 600, y: 460, width: 200, height: 200 },
      640,
      480,
      rect,
    );
    expect(placed).not.toBeNull();
    // left/top stay inside; width/height are capped to the remaining rect.
    expect(placed!.left).toBeGreaterThanOrEqual(rect.left);
    expect(placed!.left + placed!.width).toBeLessThanOrEqual(
      rect.left + rect.width + 1e-6,
    );
    expect(placed!.top + placed!.height).toBeLessThanOrEqual(
      rect.top + rect.height + 1e-6,
    );
  });

  it("returns null for a degenerate frame size", () => {
    expect(scaleBoxToRect({ x: 0, y: 0, width: 1, height: 1 }, 0, 480, rect)).toBeNull();
  });
});

describe("pickActiveBatch", () => {
  const latest = batch("cam-1", 100);
  const byCamera = { "cam-0": batch("cam-0", 90), "cam-1": latest };

  it("returns the latest batch on a single-stream node", () => {
    expect(pickActiveBatch(false, latest, byCamera, "cam-0")).toBe(latest);
    expect(pickActiveBatch(false, latest, byCamera, null)).toBe(latest);
  });

  it("returns the active leg's batch on a multi-stream node", () => {
    expect(pickActiveBatch(true, latest, byCamera, "cam-0")).toBe(byCamera["cam-0"]);
    expect(pickActiveBatch(true, latest, byCamera, "cam-1")).toBe(byCamera["cam-1"]);
  });

  it("draws nothing on a multi-stream node with no active leg or no match", () => {
    expect(pickActiveBatch(true, latest, byCamera, null)).toBeNull();
    expect(pickActiveBatch(true, latest, byCamera, "cam-9")).toBeNull();
  });
});
