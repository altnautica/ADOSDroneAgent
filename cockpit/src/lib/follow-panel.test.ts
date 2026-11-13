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
      phase: "Following",
      phaseTone: "track",
      interval: "9.6 – 10.8 m",
      geometry: "DEP 22° · BRG 064° · 1.4 m/s",
      latency: "LAT 85 ms",
      latencyWarn: false,
      mode: "Smart",
    });
    expect(m.cues).toEqual([
      { label: "GND", lit: true },
      { label: "Box", lit: false },
      { label: "LIDAR", lit: true },
    ]);
    expect(m.bar?.maxM).toBe(20);
  });

  it("reads every value as a dash and the phase as Stale past 2 s", () => {
    const m = followPanelModel(f, 2001);
    expect(m).toMatchObject({
      stale: true,
      range: "— m",
      plusMinus: null,
      phase: "Stale",
      phaseTone: "warn",
      bar: null,
      geometry: "DEP — · BRG — · — m/s",
      latency: "LAT —",
      mode: null,
    });
    expect(m.cues.every((c) => !c.lit)).toBe(true);
    expect(followPanelModel(null, 0).stale).toBe(true);
  });

  it("warns on latency above 250 ms", () => {
    const g = parseFollowState({ phase: "coasting", pipeline_latency_ms: 300, agl_source: "baro", reacquire_mode: "strict" })!;
    const m = followPanelModel(g, 0);
    expect(m).toMatchObject({ latencyWarn: true, mode: "Strict", phase: "Coasting", phaseTone: "coast" });
  });

  it("carries no second AGL: the bottom row's is the only one on screen", () => {
    const m = followPanelModel(f, 0);
    expect(m).not.toHaveProperty("agl");
    expect(m).not.toHaveProperty("aglSource");
    expect(compactFollowLine(m)).toBe("10.2 m ±0.6");
    expect(compactFollowLine(followPanelModel(null, 0))).toBe("— m");
  });
});
