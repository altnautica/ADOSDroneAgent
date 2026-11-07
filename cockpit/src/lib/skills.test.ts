import { describe, expect, it } from "vitest";

import { activateExclusive, buildBindings, echoOutcome, ECHO_TIMEOUT_MS, runningMotionSkills } from "@/lib/skill-gates";
import { armGateReason, BUILTIN_SKILL_BY_ID, BUILTIN_SKILLS, resolveSkillState, skillVisibleFor, type Skill, type SkillContext } from "@/lib/skills";

const pilot = { armed: true, landed: false, vehicleClass: "multirotor" as const, fresh: true };
const flying: SkillContext = { fcConnected: true, live: true, armed: true, pilot, pilotLinkOpen: true };

function plugin(id: string, over: Partial<NonNullable<Skill["extension"]>> = {}): Skill {
  return {
    id: `p:${id}`,
    label: id,
    category: "behavior",
    confirm: { kind: "tap", ms: 0 },
    armRequirement: "any",
    extension: {
      pluginId: "p",
      pluginName: "P",
      localId: id,
      configKey: "active",
      toggle: true,
      stateTopic: "p.state",
      motion: true,
      nudge: false,
      oneShot: false,
      vehicleClasses: ["multirotor", "fixedwing", "vtol"],
      defaultBinding: null,
      ...over,
    },
  };
}

describe("built-ins come from builtins.json", () => {
  it("has exactly the spec ids, no abort, rth not rtl", () => {
    expect(BUILTIN_SKILLS.map((s) => s.id)).toEqual(["arm", "disarm", "takeoff", "land", "rth", "pause", "resume", "take_control", "kill"]);
  });

  it("uses the spec confirm tiers", () => {
    expect(BUILTIN_SKILL_BY_ID.arm.confirm).toEqual({ kind: "hold", ms: 800 });
    expect(BUILTIN_SKILL_BY_ID.rth.confirm).toEqual({ kind: "hold", ms: 1500 });
    expect(BUILTIN_SKILL_BY_ID.pause.confirm.kind).toBe("tap");
    expect(BUILTIN_SKILL_BY_ID.kill.confirm).toEqual({ kind: "hold_confirm", ms: 3000 });
  });
});

describe("arm gates", () => {
  it("evaluates armed_landed and disarmed_or_landed", () => {
    expect(armGateReason("armed_landed", true, true)).toBeNull();
    expect(armGateReason("armed_landed", true, false)).toBe("Airborne");
    expect(armGateReason("disarmed_or_landed", false, null)).toBeNull();
    expect(armGateReason("disarmed_or_landed", true, null)).toBe("Landed state unknown");
  });

  it("gates takeoff off while airborne and disarm off while flying", () => {
    expect(resolveSkillState(BUILTIN_SKILL_BY_ID.takeoff, flying).enabled).toBe(false);
    expect(resolveSkillState(BUILTIN_SKILL_BY_ID.disarm, flying).enabled).toBe(false);
    expect(resolveSkillState(BUILTIN_SKILL_BY_ID.land, flying).enabled).toBe(true);
  });
});

describe("stale link rules", () => {
  const stale: SkillContext = { ...flying, pilot: { ...pilot, fresh: false } };
  it("sends pause and kill blind, disables rth with a reason", () => {
    expect(resolveSkillState(BUILTIN_SKILL_BY_ID.pause, stale).enabled).toBe(true);
    expect(resolveSkillState(BUILTIN_SKILL_BY_ID.kill, stale).enabled).toBe(true);
    const rth = resolveSkillState(BUILTIN_SKILL_BY_ID.rth, stale);
    expect(rth.enabled).toBe(false);
    expect(rth.reason).toBeTruthy();
  });

  it("disables take_control when no pilot runs", () => {
    const noPilot: SkillContext = { fcConnected: true, live: true, armed: true, pilot: null };
    expect(resolveSkillState(BUILTIN_SKILL_BY_ID.take_control, noPilot)).toMatchObject({ enabled: false, reason: "Pilot not running" });
  });
});

describe("plugin skill freshness and link loss", () => {
  const follow = plugin("follow");
  it("dims state older than 3 s and keeps an active skill stoppable", () => {
    const ctx: SkillContext = { ...flying, now: 10_000, reported: { [follow.id]: { state: "active", at: 5_000 } } };
    expect(resolveSkillState(follow, ctx)).toMatchObject({ enabled: true, active: true, stale: true, lastSeenS: 5 });
  });

  it("hides a skill whose vehicle classes exclude the vehicle", () => {
    expect(skillVisibleFor(plugin("x", { vehicleClasses: ["multirotor", "vtol"] }), "fixedwing")).toBe(false);
    expect(skillVisibleFor(plugin("x", { vehicleClasses: ["multirotor", "vtol"] }), null)).toBe(true);
  });
});

describe("echo and preemption", () => {
  const a = plugin("a");
  const b = plugin("b");
  it("settles a write on a later echo, expires after 3 s", () => {
    const w = { want: true, at: 1000 };
    expect(echoOutcome(w, { state: "active", at: 1500 }, 1600)).toBe("confirmed");
    expect(echoOutcome(w, { state: "active", at: 900 }, 1600)).toBe("pending");
    expect(echoOutcome(w, undefined, 1000 + ECHO_TIMEOUT_MS + 1)).toBe("expired");
  });

  it("stops the running motion skill and waits before activating the next", async () => {
    const log: string[] = [];
    const running = runningMotionSkills([a, b], { [a.id]: { state: "active" } }, {});
    expect(running.map((s) => s.id)).toEqual([a.id]);
    await activateExclusive(b, running, {
      write: async (s, v) => void log.push(`${s.id}=${v}`),
      waitIdle: async (s) => void log.push(`wait ${s.id}`),
    });
    expect(log).toEqual([`${a.id}=false`, `wait ${a.id}`, `${b.id}=true`]);
  });
});

describe("keyboard bindings", () => {
  it("gives the later skill no key on a collision and lists it", () => {
    const first = plugin("one", { defaultBinding: { key: "shift+f", gamepadButton: null } });
    const second = plugin("two", { defaultBinding: { key: "shift+f", gamepadButton: null } });
    const t = buildBindings([first, second]);
    expect(Object.entries(t.keys).filter(([, id]) => id === `skill:${first.id}` || id === `skill:${second.id}`)).toHaveLength(1);
    expect(t.conflicts.some((c) => c.actionId.endsWith(second.id))).toBe(true);
  });

  it("binds space to pause and shift+x to kill", () => {
    const t = buildBindings([]);
    expect(Object.values(t.keys)).toEqual(expect.arrayContaining(["pause", "kill"]));
  });
});
