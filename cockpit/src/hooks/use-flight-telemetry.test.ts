// The freshness and provenance rules behind every flight instrument.
//
// These were untested while being the single gate on whether the HUD draws
// anything at all: `live` false blanks the horizon, dashes both tapes and
// disables the skill bar, so a wrong answer here is indistinguishable from a
// dead aircraft. The specific bug that motivated the suite is the clock-skew
// one — the old rule compared the agent's timestamp against the browser's
// `Date.now()`, so a viewer whose clock disagreed with the agent's by more than
// four seconds saw a permanently dead cockpit over a perfectly healthy link.

import { describe, expect, it } from "vitest";

import {
  hasUsableAttitude,
  isLive,
  isRelayed,
  vehicleStamp,
} from "@/hooks/use-flight-telemetry";
import type { VehicleState } from "@/lib/types";

const STAMP = "2026-07-31T17:11:36+00:00";

function flying(over: Partial<VehicleState> = {}): VehicleState {
  return {
    armed: true,
    mode: "STABILIZE",
    attitude: { roll: 0.088, pitch: -0.037, yaw: 0.39 },
    last_update: STAMP,
    ...over,
  };
}

describe("hasUsableAttitude", () => {
  it("accepts a real attitude", () => {
    expect(hasUsableAttitude(flying())).toBe(true);
  });

  it("rejects an absent snapshot, block, or non-finite angle", () => {
    expect(hasUsableAttitude(null)).toBe(false);
    expect(hasUsableAttitude({})).toBe(false);
    expect(
      hasUsableAttitude({ attitude: { roll: null, pitch: null, yaw: null } }),
    ).toBe(false);
    expect(
      hasUsableAttitude({ attitude: { roll: NaN, pitch: 0, yaw: 0 } }),
    ).toBe(false);
  });

  it("does not require yaw, which the HUD reads from heading instead", () => {
    expect(
      hasUsableAttitude({ attitude: { roll: 0.1, pitch: 0.1, yaw: null } }),
    ).toBe(true);
  });
});

describe("isLive", () => {
  it("is true while the vehicle timestamp keeps moving", () => {
    expect(isLive(flying(), 0)).toBe(true);
    expect(isLive(flying(), 3999)).toBe(true);
  });

  it("goes false once the timestamp has stood still too long", () => {
    // An agent that keeps answering with a frozen snapshot is the case this
    // still has to catch.
    expect(isLive(flying(), 4000)).toBe(false);
    expect(isLive(flying(), 60_000)).toBe(false);
  });

  it("ignores clock skew entirely", () => {
    // The regression. The stamp is years off the client's clock in both
    // directions; what decides liveness is that it is MOVING, which the caller
    // reports as `msSinceStampMoved`.
    const future = flying({ last_update: "2030-01-01T00:00:00+00:00" });
    const past = flying({ last_update: "2001-01-01T00:00:00+00:00" });
    expect(isLive(future, 100)).toBe(true);
    expect(isLive(past, 100)).toBe(true);
  });

  it("trusts a stamp-less snapshot that carries attitude", () => {
    // The agent only emits vehicle fields once its own gates say they are
    // fresh, so with no timestamp the client has no better information and must
    // not invent staleness.
    const noStamp: VehicleState = {
      attitude: { roll: 0.1, pitch: 0.1, yaw: 0 },
    };
    expect(vehicleStamp(noStamp)).toBeNull();
    expect(isLive(noStamp, null)).toBe(true);
  });

  it("is false without attitude however fresh the snapshot is", () => {
    expect(isLive({ last_update: STAMP }, 0)).toBe(false);
    expect(isLive(null, 0)).toBe(false);
  });

  it("falls back to last_heartbeat when last_update is absent", () => {
    expect(vehicleStamp({ last_heartbeat: STAMP })).toBe(STAMP);
    expect(vehicleStamp({ last_update: STAMP, last_heartbeat: "other" })).toBe(
      STAMP,
    );
  });
});

describe("isRelayed", () => {
  it("is true only when the agent stamped the relayed provenance", () => {
    expect(isRelayed(flying({ telemetry_source: "relayed" }))).toBe(true);
    // An absent stamp means a directly attached flight controller — the
    // ordinary drone case, which the agent deliberately leaves unstamped so its
    // payload shape is unchanged.
    expect(isRelayed(flying())).toBe(false);
    expect(isRelayed(null)).toBe(false);
  });

  it("is independent of liveness, because the two answer different questions", () => {
    // A relayed aircraft with real attitude: live (draw the horizon) AND
    // relayed (do not offer to command it). Collapsing these was the defect.
    const relayed = flying({ telemetry_source: "relayed" });
    expect(isLive(relayed, 0)).toBe(true);
    expect(isRelayed(relayed)).toBe(true);
  });
});
