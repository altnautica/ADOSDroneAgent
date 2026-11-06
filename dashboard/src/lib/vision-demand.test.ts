import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

import { DETECTION_DEMAND_MODES } from "./vision-demand";

describe("detection demand options", () => {
  it("equal the vision.detection_demand enum in the generated config schema", () => {
    const schemaUrl = new URL("../../../schemas/agent-config.schema.json", import.meta.url);
    const schema = JSON.parse(readFileSync(schemaUrl, "utf-8")) as {
      $defs: { VisionConfig: { properties: { detection_demand: { enum?: string[] } } } };
    };
    const allowed = schema.$defs.VisionConfig.properties.detection_demand.enum;
    expect(allowed).toBeDefined();
    expect([...DETECTION_DEMAND_MODES]).toEqual(allowed);
  });
});
