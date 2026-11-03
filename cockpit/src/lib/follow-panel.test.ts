import { describe, expect, it } from "vitest";

import { compactFollowLine, followPanelModel } from "@/lib/follow-panel";
import { parseFollowState } from "@/shared/hud/nav-types";

const f = parseFollowState({
  phase: "following",
  range_m: 10.2,
  range_sigma_m: 0.3,
  agl_m: 4.1,
  agl_source: "rng",
  depression_deg: 22.4,
  bearing_deg: 64,
  target_speed_mps: 1.43,
  range_cues: [
    { kind: "ground", range_m: 10.1, sigma_m: 0.4 },
    { kind: "lidar", range_m: 10.2, sigma_m: 0.05 },
  ],
  pipeline_latency_ms: 85,
  reacquire_mode: "smart",
})!;

describe("followPanelModel", () => {
  it("formats every row of a fresh report", () => {
    const m = followPanelModel(f, 100);
    expect(m).toMatchObject({
      stale: false,
      range: "10.2 m",
      plusMinus: "±0.6",
      phase: "FOLLOWING",
      phaseTone: "track",
      interval: "9.6 – 10.8 m",
      agl: "4.1 m",
      aglSource: "RNG",
      geometry: "DEP 22° · BRG 064° · 1.4 m/s",
      latency: "LAT 85 ms",
      latencyWarn: false,
      mode: "SMART",
    });
    expect(m.cues).toEqual([
      { label: "GND", lit: true },
      { label: "BOX", lit: false },
      { label: "LIDAR", lit: true },
    ]);
    expect(m.bar?.maxM).toBe(20);
  });

  it("reads every value as a dash and the phase as STALE past 2 s", () => {
    const m = followPanelModel(f, 2001);
    expect(m).toMatchObject({
      stale: true,
      range: "— m",
      plusMinus: null,
      phase: "STALE",
      phaseTone: "warn",
      bar: null,
      agl: "— m",
      aglSource: null,
      geometry: "DEP — · BRG — · — m/s",
      latency: "LAT —",
      mode: null,
    });
    expect(m.cues.every((c) => !c.lit)).toBe(true);
    expect(followPanelModel(null, 0).stale).toBe(true);
  });

  it("warns on latency above 250 ms and reads BARO", () => {
    const g = parseFollowState({ phase: "coasting", pipeline_latency_ms: 300, agl_source: "baro", reacquire_mode: "strict" })!;
    const m = followPanelModel(g, 0);
    expect(m).toMatchObject({ latencyWarn: true, aglSource: "BARO", mode: "STRICT", phase: "COASTING", phaseTone: "coast" });
  });

  it("merges rows 1 and 3 into the compact line", () => {
    expect(compactFollowLine(followPanelModel(f, 0))).toBe("10.2 m ±0.6 · AGL 4.1 RNG");
    expect(compactFollowLine(followPanelModel(null, 0))).toBe("— m · AGL —");
  });
});
