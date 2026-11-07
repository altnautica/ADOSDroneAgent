import { describe, expect, it } from "vitest";

import { layerFrames } from "@/lib/layer-frames";
import { ingestLayerFrame, parseNavFrame } from "@/lib/nav-stream";

const IDENT = [1, 0, 0, 0, 1, 0, 0, 0, 1];

describe("parseNavFrame", () => {
  it("parses each kind's payload", () => {
    expect(parseNavFrame({ kind: "pose", data: { R: IDENT, t: [1, 2, 3], health: "good" } })).toEqual({
      kind: "pose",
      data: { R: IDENT, t: [1, 2, 3], health: "good", frame: "map_ned", source: null },
    });
    const scan = parseNavFrame({ kind: "scan", data: { angle_start_rad: 0, angle_increment_rad: 0.1, ranges_m: [1, "x"] } });
    expect(scan).toEqual({
      kind: "scan",
      data: { frame: "body_frd", angleStartRad: 0, angleIncrementRad: 0.1, ranges: [1, 0] },
    });
    expect(parseNavFrame({ kind: "nav", data: { mode: "external_nav", active_set: 1, extnav_health: "good" } })).toMatchObject({
      kind: "nav",
      data: { mode: "external_nav", activeSet: 1, extnavHealth: "good" },
    });
    expect(parseNavFrame({ kind: "follow", data: { phase: "coasting", coast_remaining_s: 1.4 } })).toMatchObject({
      kind: "follow",
      data: { phase: "coasting", coastRemainingS: 1.4 },
    });
    expect(parseNavFrame({ kind: "map", data: {} })?.kind).toBe("map");
  });

  it("parses discrete follow events from the frame's topic", () => {
    expect(
      parseNavFrame({ kind: "follow_event", topic: "follow.reacquired", data: { track_id: 15, similarity: 0.86, gap_s: 1.8 } }),
    ).toEqual({ kind: "follow_event", data: { topic: "follow.reacquired", trackId: 15, similarity: 0.86, gapS: 1.8 } });
    expect(parseNavFrame({ kind: "follow_event", topic: "follow.lost", data: {} })).toEqual({
      kind: "follow_event",
      data: { topic: "follow.lost", trackId: null, reason: "lock_lost" },
    });
    expect(parseNavFrame({ kind: "follow_event", topic: "follow.reacquired", data: {} })).toBeNull();
    expect(parseNavFrame({ kind: "follow_event", topic: "other", data: {} })).toBeNull();
  });

  it("drops keepalives, unknown kinds and malformed payloads without throwing", () => {
    expect(parseNavFrame({ kind: "keepalive" })).toBeNull();
    expect(parseNavFrame({ kind: "weather", data: {} })).toBeNull();
    expect(parseNavFrame(null)).toBeNull();
    expect(parseNavFrame("pose")).toBeNull();
    expect(parseNavFrame({ kind: "pose", data: { R: [1, 0], t: [0, 0, 0] } })).toBeNull();
    expect(parseNavFrame({ kind: "scan", data: { angle_start_rad: 0, angle_increment_rad: 0, ranges_m: [] } })).toBeNull();
    expect(parseNavFrame({ kind: "nav", data: { active_set: 1 } })).toBeNull();
    expect(parseNavFrame({ kind: "follow", data: { phase: "dancing" } })).toBeNull();
  });
});

describe("cockpit v2 kinds", () => {
  it("parses a pilot frame into the store shape", () => {
    const f = parseNavFrame({ kind: "pilot", ts_ms: 1, data: { owner: "pilot", armed: true, landed: false, control_mode: "ados" } });
    expect(f?.kind).toBe("pilot");
    expect(f && f.kind === "pilot" ? f.data.controlMode : null).toBe("ados");
    expect(parseNavFrame({ kind: "pilot", data: { owner: "nobody" } })).toBeNull();
  });

  it("routes per-frame layer kinds into the mutable frame store, not the nav store", () => {
    layerFrames.clear();
    const v0 = layerFrames.version;
    expect(
      ingestLayerFrame({ kind: "ar", data: { v: 1, ts_ms: 5, approx: false, reason: null, cameras: { main: { frame_w: 4, frame_h: 3, items: [] } } } }),
    ).toBe(true);
    expect(layerFrames.ar.data?.cameras.main.frameW).toBe(4);
    expect(ingestLayerFrame({ kind: "obstacles", data: { sectors_m: [], cameras: {} } })).toBe(true);
    expect(layerFrames.obstacles.data?.sectorsM).toHaveLength(72);
    expect(layerFrames.version).toBeGreaterThan(v0);
    expect(ingestLayerFrame({ kind: "pose", data: {} })).toBe(false);
    expect(parseNavFrame({ kind: "ar", data: {} })).toBeNull();
  });
});
