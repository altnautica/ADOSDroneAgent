import { describe, expect, it } from "vitest";

import { onGround } from "./on-ground";

const NOW = 100_000;

function pilot(ageMs: number, data: { landed: boolean; armed: boolean }) {
  return { at: NOW - ageMs, data };
}

describe("onGround", () => {
  it("reads a fresh pilot's landed", () => {
    expect(onGround(pilot(200, { landed: true, armed: true }), true, NOW)).toBe(true);
    expect(onGround(pilot(200, { landed: false, armed: true }), true, NOW)).toBe(false);
  });

  it("ignores a 2.5 s old landed frame and falls back to the armed telemetry", () => {
    expect(onGround(pilot(2500, { landed: true, armed: false }), true, NOW)).toBe(false);
    expect(onGround(pilot(2500, { landed: false, armed: true }), false, NOW)).toBe(true);
  });

  it("reads disarmed telemetry as on the ground with no pilot", () => {
    expect(onGround(null, false, NOW)).toBe(true);
    expect(onGround(null, true, NOW)).toBe(false);
  });
});
