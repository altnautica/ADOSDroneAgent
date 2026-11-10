import { describe, expect, it } from "vitest";

import layersJson from "@/shared/cockpit-spec/layers.json";
import { chipTexts } from "@/shared/hud/layer-engine";
import { batchStage, DETECTION_DIM_ALPHA, DETECTION_DIM_MS, DETECTION_STALE_MS, targetsUnavailable } from "@/stores/detections-store";
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
