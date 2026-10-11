import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

import { VISION_BACKENDS } from "./vision-backends";

describe("vision backend options", () => {
  it("equal the vision.backend enum in the generated config schema", () => {
    const schemaUrl = new URL("../../../schemas/agent-config.schema.json", import.meta.url);
    const schema = JSON.parse(readFileSync(schemaUrl, "utf-8")) as {
      $defs: { VisionConfig: { properties: { backend: { enum?: string[] } } } };
    };
    const allowed = schema.$defs.VisionConfig.properties.backend.enum;
    expect(allowed).toBeDefined();
    expect([...VISION_BACKENDS]).toEqual(allowed);
  });
});
