import { afterEach, describe, expect, it } from "vitest";

import { COMMANDED_MODE_WINDOW_MS, fcSideFailsafe, forgetCommandedModes, noteCommandedModes } from "@/lib/fc-failsafe";
import { BUILTIN_SKILL_BY_ID, resolveSkillState, type SkillContext } from "@/lib/skills";
import { specText } from "@/shared/hud/spec-text";

const T0 = 1_000_000;
const stale = { armed: true, landed: false, vehicleClass: "multirotor" as const, fresh: false, fcFailsafe: false };
const ctx = (mode: string, now: number): SkillContext => ({
  fcConnected: true,
  live: true,
  armed: true,
  pilot: stale,
  pilotLinkOpen: true,
  fc: { mode, systemStatus: 4 },
  now,
});
const pause = BUILTIN_SKILL_BY_ID.pause!;

afterEach(() => forgetCommandedModes());

describe("the flight controller's own failsafe, read from its mode and heartbeat", () => {
  it("counts a return or landing this cockpit did not command", () => {
    expect(fcSideFailsafe({ mode: "RTL", systemStatus: 4 }, T0)).toBe(true);
    expect(fcSideFailsafe({ mode: "smart_rtl", systemStatus: 4 }, T0)).toBe(true);
    expect(fcSideFailsafe({ mode: "LOITER", systemStatus: 4 }, T0)).toBe(false);
    for (const mode of ["AUTO.RTL", "AUTO.LAND", "AUTO.PRECLAND", "AUTO_RTL", "LOITER_ALT_QLAND"]) {
      expect(fcSideFailsafe({ mode, systemStatus: 4 }, T0)).toBe(true);
    }
    expect(fcSideFailsafe({ mode: "AUTO.LOITER", systemStatus: 4 }, T0)).toBe(false);
    expect(fcSideFailsafe({ mode: null, systemStatus: 6 }, T0)).toBe(true);
    expect(fcSideFailsafe(null, T0)).toBe(false);
  });

  it("lets a pause through during a return this cockpit commanded, for as long as the mode holds", () => {
    noteCommandedModes(["RTL"], T0);
    expect(resolveSkillState(pause, ctx("RTL", T0 + 500)).enabled).toBe(true);
    // Still ours a minute later: the mode never left.
    expect(resolveSkillState(pause, ctx("RTL", T0 + 60_000)).enabled).toBe(true);
  });

  it("refuses once the mode left and the flight controller came back to it on its own", () => {
    noteCommandedModes(["RTL"], T0);
    expect(fcSideFailsafe({ mode: "RTL", systemStatus: 4 }, T0 + 500)).toBe(false);
    expect(fcSideFailsafe({ mode: "LOITER", systemStatus: 4 }, T0 + 20_000)).toBe(false);
    expect(resolveSkillState(pause, ctx("RTL", T0 + 30_000))).toEqual({ enabled: false, reason: specText("refusal.failsafe") });
  });

  it("does not claim a return that only started after the command's window", () => {
    noteCommandedModes(["LAND"], T0);
    expect(fcSideFailsafe({ mode: "LAND", systemStatus: 4 }, T0 + COMMANDED_MODE_WINDOW_MS + 1)).toBe(true);
  });

  it("never lets a pause through a critical heartbeat, commanded or not", () => {
    noteCommandedModes(["RTL"], T0);
    expect(fcSideFailsafe({ mode: "RTL", systemStatus: 5 }, T0 + 100)).toBe(true);
  });
});
