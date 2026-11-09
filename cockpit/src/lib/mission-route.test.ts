import { describe, expect, it } from "vitest";

import { parseMissionRoute } from "@/lib/mission-route";

const item = (seq: number, command: number, x: number, y: number, frame = 3) => ({
  param1: 0, param2: 0, param3: 0, param4: 0, x, y, z: 30, seq, command, frame,
  current: 0, autocontinue: 1, mission_type: 0,
});

describe("parseMissionRoute", () => {
  it("keeps positioned items in seq order and skips the home slot", () => {
    const body = {
      rev: 2, present: true, count: 5, home_slot: true,
      items: [
        item(2, 16, 470010000, 85010000),
        item(0, 16, 470000000, 85000000, 0),
        item(1, 22, 470005000, 85005000),
        item(3, 178, 0, 0, 2), // DO_CHANGE_SPEED: no position
        item(4, 21, 470020000, 85020000),
      ],
    };
    expect(parseMissionRoute(body)).toEqual([
      { seq: 1, lat: 47.0005, lon: 8.5005 },
      { seq: 2, lat: 47.001, lon: 8.501 },
      { seq: 4, lat: 47.002, lon: 8.502 },
    ]);
  });

  it("is empty before the router mirrored anything", () => {
    expect(parseMissionRoute({ rev: 0, present: false, count: 0, home_slot: null, items: [] })).toEqual([]);
    expect(parseMissionRoute(null)).toEqual([]);
  });

  it("drops local-frame and unset positions", () => {
    const body = { present: true, home_slot: false, items: [item(0, 16, 5, 5, 1), item(1, 16, 0, 0)] };
    expect(parseMissionRoute(body)).toEqual([]);
  });
});
