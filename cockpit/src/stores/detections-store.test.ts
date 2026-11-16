import { describe, expect, it } from "vitest";

import layersJson from "@/shared/cockpit-spec/layers.json";
import { chipTexts } from "@/shared/hud/layer-engine";
import {
  batchStage,
  DETECTION_DIM_ALPHA,
  DETECTION_DIM_MS,
  DETECTION_STALE_MS,
  targetsUnavailable,
  useDetectionsStore,
} from "@/stores/detections-store";
import { useLayerChipsStore } from "@/stores/layer-chips-store";

describe("targets layer staleness", () => {
  it("reads the dim and hide ages from the layer spec", () => {
    expect(DETECTION_DIM_MS).toBe(layersJson.staleness.dim_after_ms);
    expect(DETECTION_STALE_MS).toBe(layersJson.staleness.hide_after_ms);
    expect(DETECTION_DIM_ALPHA).toBe(0.5);
  });

  it("draws a batch in full, dims it from 500 ms and hides it from 2 s", () => {
    const at = 10_000;
    expect(batchStage(at, at)).toBe("fresh");
    expect(batchStage(at, at + 500)).toBe("fresh");
    expect(batchStage(at, at + 501)).toBe("dim");
    expect(batchStage(at, at + 2000)).toBe("dim");
    expect(batchStage(at, at + 2001)).toBe("hidden");
    expect(batchStage(null, at)).toBe("hidden");
  });

  it("calls targets unavailable only while a detector runs and its batch is hidden", () => {
    expect(targetsUnavailable("hidden", true)).toBe(true);
    expect(targetsUnavailable("dim", true)).toBe(false);
    expect(targetsUnavailable("fresh", true)).toBe(false);
    expect(targetsUnavailable("hidden", false)).toBe(false);
  });

  it("stacks the targets chip with the layer engine's chips", () => {
    const s = useLayerChipsStore.getState();
    s.setMask(0);
    s.setTargetsUnavailable(true);
    expect(chipTexts(useLayerChipsStore.getState().mask)).toEqual(["Targets unavailable"]);
    // The engine's next report keeps the targets chip.
    s.setMask(1 << 3);
    expect(chipTexts(useLayerChipsStore.getState().mask)).toEqual(["Obstacles unavailable", "Targets unavailable"]);
    s.setTargetsUnavailable(false);
    expect(chipTexts(useLayerChipsStore.getState().mask)).toEqual(["Obstacles unavailable"]);
  });
});

describe("crop-only batches hold the whole-frame boxes outside the crop", () => {
  const box = (x: number, trackId: number) => ({
    bbox: { x, y: 100, width: 40, height: 100 },
    classLabel: "person",
    confidence: 0.9,
    trackId,
    lockState: null,
  });
  const batch = (tsMs: number, detections: ReturnType<typeof box>[], cropWindow?: { x: number; y: number; width: number; height: number }) => ({
    modelId: "m",
    cameraId: "main",
    frameId: tsMs,
    tsMs,
    frameWidth: 640,
    frameHeight: 480,
    detections,
    cropWindow,
  });
  const ids = () => useDetectionsStore.getState().latest!.detections.map((d) => d.trackId);
  const crop = { x: 200, y: 50, width: 200, height: 200 };

  it("draws the crop's detections plus the whole frame's boxes centred outside it", () => {
    const s = useDetectionsStore.getState();
    s.clear();
    s.setBatch(batch(1000, [box(20, 1), box(280, 2), box(560, 3)]));
    s.setBatch(batch(1100, [box(290, 2)], crop));
    // 2 comes from the crop; 1 and 3 lie outside it and are held.
    expect(ids()).toEqual([2, 1, 3]);
    // A held box inside the window is not drawn: the crop speaks for it.
    s.setBatch(batch(1200, [], crop));
    expect(ids()).toEqual([1, 3]);
  });

  it("drops a held box whose track the crop reports, and expires the hold after 1 s", () => {
    const s = useDetectionsStore.getState();
    s.clear();
    s.setBatch(batch(1000, [box(20, 1), box(560, 3)]));
    s.setBatch(batch(1100, [box(20, 1)], crop));
    expect(ids()).toEqual([1, 3]);
    s.setBatch(batch(2001, [box(290, 2)], crop));
    expect(ids()).toEqual([2]);
  });

  it("lets a whole-frame batch replace everything", () => {
    const s = useDetectionsStore.getState();
    s.clear();
    s.setBatch(batch(1000, [box(20, 1)]));
    s.setBatch(batch(1100, [box(560, 3)]));
    expect(ids()).toEqual([3]);
    s.clear();
  });
});
