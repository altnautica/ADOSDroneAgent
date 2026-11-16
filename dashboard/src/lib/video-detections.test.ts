import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { holdDetectionLease, holdOutsideCrop, layoutVideoBoxes, parseLatestDetections, type VisibilitySource } from "@/lib/video-detections";

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

describe("detection lease while the page is hidden", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("renews only while visible, releases on hide and takes it again on show", () => {
    const listeners = new Set<() => void>();
    const doc = {
      visibilityState: "visible" as DocumentVisibilityState,
      addEventListener: (_: "visibilitychange", fn: () => void) => void listeners.add(fn),
      removeEventListener: (_: "visibilitychange", fn: () => void) => void listeners.delete(fn),
    } satisfies VisibilitySource;
    const flip = (s: DocumentVisibilityState) => {
      doc.visibilityState = s;
      for (const fn of listeners) fn();
    };
    const calls: number[] = [];
    const stop = holdDetectionLease((h) => calls.push(h), doc, 6, 2000);
    vi.advanceTimersByTime(4000);
    expect(calls).toEqual([6, 6, 6]);
    flip("hidden");
    expect(calls).toEqual([6, 6, 6, 0]);
    vi.advanceTimersByTime(60_000);
    expect(calls).toEqual([6, 6, 6, 0]);
    flip("visible");
    expect(calls).toEqual([6, 6, 6, 0, 6]);
    stop();
    expect(calls).toEqual([6, 6, 6, 0, 6, 0]);
    expect(listeners.size).toBe(0);
  });
});

describe("crop-only batches hold the whole-frame boxes outside the crop", () => {
  const det = (x: number, track_id: number) => ({ bbox: { x, y: 100, width: 40, height: 100 }, class_label: "person", track_id });
  const frame = (ts_ms: number, dets: ReturnType<typeof det>[], crop_window?: Record<string, number>) =>
    parseLatestDetections({ available: true, camera_id: "main", ts_ms, detections: dets, crop_window }, "main")!;
  const crop = { x: 200, y: 50, width: 200, height: 200 };
  const ids = (f: { boxes: { trackId: number | null }[] }) => f.boxes.map((b) => b.trackId);

  it("parses the crop window and the capture stamp", () => {
    expect(frame(1100, [], crop).cropWindow).toEqual({ x: 200, y: 50, w: 200, h: 200 });
    expect(frame(1100, []).cropWindow).toBeNull();
    expect(frame(1100, []).tsMs).toBe(1100);
  });

  it("adds held boxes centred outside the window, minus tracks the crop reports, for 1 s", () => {
    const full = frame(1000, [det(20, 1), det(280, 2), det(560, 3)]);
    expect(ids(holdOutsideCrop(full, frame(1100, [det(290, 2)], crop)))).toEqual([2, 1, 3]);
    expect(ids(holdOutsideCrop(full, frame(1100, [det(20, 1)], crop)))).toEqual([1, 3]);
    expect(ids(holdOutsideCrop(full, frame(2001, [det(290, 2)], crop)))).toEqual([2]);
    expect(ids(holdOutsideCrop(full, frame(1100, [det(560, 3)])))).toEqual([3]);
  });
});

describe("a crop-only reply carries the whole-frame batch", () => {
  it("parses full_frame so a poller that never saw it holds its boxes", () => {
    const det = { bbox: { x: 20, y: 100, width: 40, height: 100 }, class_label: "person", track_id: 1 };
    const f = parseLatestDetections(
      {
        available: true,
        camera_id: "main",
        ts_ms: 1100,
        detections: [],
        crop_window: { x: 200, y: 50, width: 200, height: 200 },
        full_frame: { camera_id: "main", ts_ms: 1000, detections: [det] },
      },
      "main",
    )!;
    expect(f.fullFrame?.tsMs).toBe(1000);
    expect(holdOutsideCrop(f.fullFrame!, f).boxes.map((b) => b.trackId)).toEqual([1]);
  });
});
