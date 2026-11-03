import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

import { stepLeafChange } from "@/lib/settings/guarded-leaf";
import { VISION_BACKENDS, confidenceToPct, pctToConfidence, toBackend } from "@/lib/settings/vision";

describe("vision backends", () => {
  it("match the config schema enum", () => {
    const schemaPath = path.resolve(__dirname, "../../../../schemas/agent-config.schema.json");
    const schema = JSON.parse(readFileSync(schemaPath, "utf8")) as {
      $defs: { VisionConfig: { properties: { backend: { enum: string[] } } } };
    };
    expect([...VISION_BACKENDS]).toEqual(schema.$defs.VisionConfig.properties.backend.enum);
  });

  it("reads an absent backend as auto", () => {
    expect(toBackend(undefined)).toBe("auto");
    expect(toBackend("hailo")).toBe("hailo");
  });
});

describe("vision confidence", () => {
  it("edits the fraction as a whole percent and writes it back", () => {
    expect(confidenceToPct(0.5)).toBe(50);
    expect(confidenceToPct(0.425)).toBe(43);
    expect(confidenceToPct(1.7)).toBe(100);
    expect(pctToConfidence(35)).toBe(0.35);
    expect(pctToConfidence(-5)).toBe(0);
  });

  it("writes without a guard", () => {
    expect(stepLeafChange({ key: "vision.enabled", value: true }, false).kind).toBe("write");
  });
});
