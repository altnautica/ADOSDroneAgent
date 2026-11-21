import { describe, expect, it } from "vitest";

import { parsePilotState } from "./cockpit-frames";
import { parseSensing } from "./pilot-fields";
import { sensingChipText } from "./sensing";

describe("sensing chip", () => {
  it("parses the pilot's sensing and words its tier", () => {
    const s = parseSensing({ tier: "lidar3d", clear_range_m: 11.6, speed_limit_mps: 4.24 })!;
    expect(s).toEqual({ tier: "lidar3d", clearRangeM: 11.6, speedLimitMps: 4.24 });
    expect(sensingChipText(s)).toBe("Sensing: 3D lidar · ≤ 4.2 m/s");
  });

  it("labels every tier", () => {
    const text = (tier: string) => sensingChipText(parseSensing({ tier, clear_range_m: 6, speed_limit_mps: 3 }));
    expect(text("depth")).toBe("Sensing: Depth camera · ≤ 3 m/s");
    expect(text("camera")).toBe("Sensing: Camera · ≤ 3 m/s");
    expect(text("planar")).toBe("Sensing: 2D lidar · ≤ 3 m/s");
  });

  it("shows nothing when the pilot reports no sensing or an unknown tier", () => {
    expect(parseSensing(undefined)).toBeNull();
    expect(parseSensing({ tier: "radar", clear_range_m: 6, speed_limit_mps: 3 })).toBeNull();
    expect(parseSensing({ tier: "planar", clear_range_m: 6 })).toBeNull();
    expect(sensingChipText(null)).toBeNull();
  });

  it("rides on the pilot state", () => {
    const base = { owner: "pilot", fc_mode: 4, armed: true };
    expect(parsePilotState(base)?.sensing ?? null).toBeNull();
    const p = parsePilotState({ ...base, sensing: { tier: "depth", clear_range_m: 8, speed_limit_mps: 3.1 } });
    expect(p?.sensing).toEqual({ tier: "depth", clearRangeM: 8, speedLimitMps: 3.1 });
  });
});
