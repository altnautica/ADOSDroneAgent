import { describe, expect, it } from "vitest";

import {
  AUTOPILOT_PX4,
  CORE_SKILLS,
  modePresetsFor,
  resolveSkillState,
  type Skill,
} from "@/lib/skills";

function skill(id: string): Skill {
  const found = CORE_SKILLS.find((s) => s.id === id);
  if (!found) throw new Error(`no core skill ${id}`);
  return found;
}

describe("resolveSkillState", () => {
  it("disables every skill without a live FC link", () => {
    for (const s of CORE_SKILLS) {
      const state = resolveSkillState(s, { fcConnected: false, armed: false });
      expect(state.enabled).toBe(false);
      expect(state.reason).toBe("No flight controller link");
    }
  });

  it("names the real reason for a relayed vehicle instead of blaming the link", () => {
    // A relayed aircraft is not undrivable because the link is dead — it is
    // drivable, just not from this node. Saying "no flight controller link"
    // while a live horizon moves in front of the operator is the kind of
    // contradiction that trains distrust of every other reading on screen.
    for (const s of CORE_SKILLS) {
      const state = resolveSkillState(s, {
        fcConnected: false,
        armed: false,
        relayed: true,
      });
      expect(state.enabled).toBe(false);
      expect(state.reason).toBe("Relayed vehicle — command it from its own node");
    }
  });

  it("still drives skills normally on a directly attached FC", () => {
    // The relayed flag must not leak into the attached case: `relayed` is only
    // consulted once the command path is already gated off.
    const state = resolveSkillState(CORE_SKILLS[0], {
      fcConnected: true,
      armed: false,
      relayed: false,
    });
    expect(state.reason).not.toBe("Relayed vehicle — command it from its own node");
  });

  it("disables arm while armed, and disarm while disarmed", () => {
    const armedCtx = { fcConnected: true, armed: true };
    const disarmedCtx = { fcConnected: true, armed: false };
    expect(resolveSkillState(skill("arm"), armedCtx)).toEqual({
      enabled: false,
      reason: "Already armed",
    });
    expect(resolveSkillState(skill("disarm"), disarmedCtx)).toEqual({
      enabled: false,
      reason: "Not armed",
    });
  });

  it("enables arm when disarmed, and disarm when armed", () => {
    expect(resolveSkillState(skill("arm"), { fcConnected: true, armed: false })).toEqual({
      enabled: true,
    });
    expect(resolveSkillState(skill("disarm"), { fcConnected: true, armed: true })).toEqual({
      enabled: true,
    });
  });

  it("enables takeoff / land / rtl whenever the FC link is live", () => {
    const ctx = { fcConnected: true, armed: true };
    for (const id of ["takeoff", "land", "rtl"]) {
      expect(resolveSkillState(skill(id), ctx).enabled).toBe(true);
    }
  });

  it("marks the high-consequence core actions as confirm-guarded", () => {
    for (const s of CORE_SKILLS) {
      expect(s.confirm).toBe(true);
    }
  });
});

describe("modePresetsFor", () => {
  it("offers the ArduPilot mode names by default (unknown autopilot)", () => {
    const names = modePresetsFor(null).map((s) => s.args[0]);
    expect(names).toEqual(["STABILIZE", "ALT_HOLD", "LOITER", "GUIDED"]);
  });

  it("offers the PX4 mode names for a PX4 autopilot", () => {
    const names = modePresetsFor(AUTOPILOT_PX4).map((s) => s.args[0]);
    expect(names).toEqual(["ALTITUDE", "POSITION", "LOITER", "MISSION"]);
  });

  it("emits mode skills as non-confirm `mode` commands carrying the name arg", () => {
    for (const s of modePresetsFor(null)) {
      expect(s.cmd).toBe("mode");
      expect(s.confirm).toBe(false);
      expect(s.category).toBe("mode");
      expect(s.args).toHaveLength(1);
    }
  });

  it("gates mode presets off without a live FC link", () => {
    const s = modePresetsFor(null)[0];
    expect(resolveSkillState(s, { fcConnected: false, armed: false }).enabled).toBe(false);
    expect(resolveSkillState(s, { fcConnected: true, armed: false }).enabled).toBe(true);
  });
});
