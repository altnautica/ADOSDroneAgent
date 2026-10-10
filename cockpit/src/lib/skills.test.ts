import { describe, expect, it } from "vitest";

import { AUTOPILOT_PX4, CORE_BY_ID, CORE_SKILLS, modePresetsFor, resolveSkillState } from "@/lib/skills";
import { parseExtensionSkill } from "@/lib/extensions";

const live = { fcConnected: true, live: true };

describe("command skills need a commandable FC link with live telemetry", () => {
  it("disables every built-in without an FC link", () => {
    for (const s of CORE_SKILLS) {
      expect(resolveSkillState(s, { fcConnected: false, live: true, armed: true })).toEqual({
        enabled: false,
        reason: "No flight controller link",
      });
    }
  });

  it("disables every built-in when the link answers but telemetry is not live", () => {
    // A relayed reading that has gone stale, or a link with no vehicle state.
    for (const s of CORE_SKILLS) {
      expect(resolveSkillState(s, { fcConnected: true, live: false, armed: true }).enabled).toBe(false);
    }
  });

  it("enables commands on a live link", () => {
    expect(resolveSkillState(CORE_BY_ID.arm, { ...live, armed: false }).enabled).toBe(true);
    for (const id of ["disarm", "takeoff", "land", "rtl", "pause", "resume", "kill"]) {
      expect(resolveSkillState(CORE_BY_ID[id], { ...live, armed: true }).enabled).toBe(true);
    }
  });

  it("applies the arm requirement", () => {
    expect(resolveSkillState(CORE_BY_ID.arm, { ...live, armed: true }).reason).toBe("Already armed");
    expect(resolveSkillState(CORE_BY_ID.land, { ...live, armed: false }).reason).toBe("Not armed");
  });
});

describe("confirm tiers", () => {
  it("maps the built-ins to their gestures", () => {
    expect(CORE_BY_ID.arm.gesture).toBe("slide");
    expect(CORE_BY_ID.kill.gesture).toBe("guarded");
    expect(CORE_BY_ID.pause.gesture).toBe("tap");
    for (const id of ["disarm", "takeoff", "land", "rtl", "resume"]) {
      expect(CORE_BY_ID[id].gesture).toBe("hold");
    }
    expect(CORE_BY_ID.takeoff.takesAltitude).toBe(true);
    expect(CORE_BY_ID.kill.command?.cmd).toBe("killswitch");
  });

  it("taps recovery modes and holds the rest", () => {
    const byName = Object.fromEntries(modePresetsFor(null).map((s) => [s.command?.args[0], s.gesture]));
    expect(byName.LOITER).toBe("tap");
    expect(byName.ALT_HOLD).toBe("tap");
    expect(byName.GUIDED).toBe("hold");
    expect(modePresetsFor(AUTOPILOT_PX4).map((s) => s.command?.args[0])).toEqual([
      "ALTITUDE",
      "POSITION",
      "LOITER",
      "MISSION",
    ]);
  });
});

describe("extension skills", () => {
  const follow = parseExtensionSkill("com.example.follow", {
    id: "follow",
    label: "Follow",
    toggle: true,
    confirm: true,
    arm_requirement: "armed",
    activation: { via: "config", config_key: "active" },
    state: { via: "event", topic: "follow.state" },
  })!;

  it("parses a config-activated skill", () => {
    expect(follow).toMatchObject({
      id: "com.example.follow:follow",
      gesture: "hold",
      armRequirement: "armed",
      extension: { pluginId: "com.example.follow", configKey: "active", toggle: true, stateTopic: "follow.state" },
    });
  });

  it("follows the plugin's reported state", () => {
    const ctx = { ...live, armed: true };
    expect(
      resolveSkillState(follow, { ...ctx, reported: { [follow.id]: { state: "disabled", reason: "No target" } } }),
    ).toEqual({ enabled: false, reason: "No target" });
    expect(resolveSkillState(follow, { ...ctx, reported: { [follow.id]: { state: "active" } } })).toMatchObject({
      enabled: true,
      active: true,
    });
  });
});
