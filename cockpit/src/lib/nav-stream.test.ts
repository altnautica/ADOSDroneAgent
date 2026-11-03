import { describe, expect, it } from "vitest";

import { parseNavFrame } from "@/lib/nav-stream";

const IDENT = [1, 0, 0, 0, 1, 0, 0, 0, 1];

describe("parseNavFrame", () => {
  it("parses each kind's payload", () => {
    expect(parseNavFrame({ kind: "pose", data: { R: IDENT, t: [1, 2, 3], health: "good" } })).toEqual({
      kind: "pose",
      data: { R: IDENT, t: [1, 2, 3], health: "good", frame: "map_frd", source: null },
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
