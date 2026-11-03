import { describe, expect, it } from "vitest";

import { buildVideoOverlayProps, videoOverlayPushKey, type VideoOverlayInput } from "@/lib/video-overlay-props";
import type { CockpitDetectionBatch } from "@/stores/detections-store";

const batch: CockpitDetectionBatch = {
  modelId: "m",
  cameraId: "main",
  frameId: 41,
  tsMs: 123_456,
  frameWidth: 1280,
  frameHeight: 720,
  receivedAt: 9000,
  detections: [
    { bbox: { x: 10, y: 20, width: 30, height: 40 }, classLabel: "person", confidence: 0.9, trackId: 7, lockState: "locked" },
    { classLabel: "person", confidence: 0.5 },
  ],
};

const input: VideoOverlayInput = {
  droneId: "local",
  cameraId: "main",
  videoWidth: 1920,
  videoHeight: 1080,
  rendered: { left: 0, top: 37.53, width: 800, height: 450.04 },
  batch,
  attitude: { roll: Math.PI / 18, pitch: -Math.PI / 36, yaw: Math.PI / 2 },
};

describe("buildVideoOverlayProps", () => {
  it("carries the contract fields", () => {
    const p = buildVideoOverlayProps(input);
    expect(p).toMatchObject({
      droneId: "local",
      cameraId: "main",
      streamWidth: 1920,
      streamHeight: 1080,
      renderedRect: { left: 0, top: 37.5, width: 800, height: 450 },
      frameTimestampMs: 9000,
      frameAgeMs: null,
      attitudeAtFrameTime: false,
    });
    expect(p.attitude?.rollDeg).toBeCloseTo(10);
    expect(p.attitude?.pitchDeg).toBeCloseTo(-5);
    expect(p.attitude?.yawDeg).toBeCloseTo(90);
    expect(p.detections).toEqual({
      frameWidth: 1280,
      frameHeight: 720,
      frameId: 41,
      receivedAt: 9000,
      items: [{ bbox: { x: 10, y: 20, width: 30, height: 40 }, classLabel: "person", confidence: 0.9, trackId: 7, lockState: "locked" }],
    });
  });

  it("pushes null detections when the batch aged out, and null attitude when unknown", () => {
    const p = buildVideoOverlayProps({ ...input, batch: null, attitude: null, videoWidth: null, videoHeight: null });
    expect(p.detections).toBeNull();
    expect(p.attitude).toBeNull();
    expect(buildVideoOverlayProps({ ...input, attitude: { roll: 0, pitch: null, yaw: 0 } }).attitude).toBeNull();
    expect(p.streamWidth).toBe(0);
    expect(p.frameTimestampMs).toBe(0);
  });

  it("falls back to the batch frame size before video metadata", () => {
    const p = buildVideoOverlayProps({ ...input, videoWidth: null, videoHeight: null });
    expect([p.streamWidth, p.streamHeight]).toEqual([1280, 720]);
  });
});

describe("videoOverlayPushKey", () => {
  const key = (over: Partial<VideoOverlayInput>) => videoOverlayPushKey(buildVideoOverlayProps({ ...input, ...over }));

  it("changes on a new batch, a stale batch, a resize or a camera switch", () => {
    const k = key({});
    expect(key({ batch: { ...batch, frameId: 42 } })).not.toBe(k);
    expect(key({ batch: null })).not.toBe(k);
    expect(key({ rendered: { ...input.rendered, width: 801 } })).not.toBe(k);
    expect(key({ cameraId: "belly" })).not.toBe(k);
  });

  it("ignores a changing attitude, but pushes when it goes unknown", () => {
    expect(key({ attitude: { roll: 1, pitch: 1, yaw: 1 } })).toBe(key({}));
    expect(key({ attitude: null })).not.toBe(key({}));
  });
});
