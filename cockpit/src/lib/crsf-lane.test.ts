import { describe, expect, it } from "vitest";

import { crsfLaneEnabled } from "@/shared/api/lora";

describe("crsfLaneEnabled", () => {
  it("reads the RC lane status only when the lane is configured on", () => {
    expect(crsfLaneEnabled({ radio: { crsf: { enabled: true } } })).toBe(true);
    expect(crsfLaneEnabled({ radio: { crsf: { enabled: false } } })).toBe(false);
    expect(crsfLaneEnabled({})).toBe(false);
    expect(crsfLaneEnabled(null)).toBe(false);
  });
});
