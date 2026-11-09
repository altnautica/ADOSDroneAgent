import { describe, expect, it } from "vitest";

import { effectiveLayers, LAYER_LEVELS, nextLevel, nextView, pluginLayerId, prevLevel } from "@/lib/cockpit-view";
import { cockpitText } from "@/lib/cockpit-text";
import { stepProfile } from "@/lib/feed-actions";
import { flightStateChip, type ChipFacts } from "@/shared/hud/flight-state";
import { checklistPassable, launchArgs, visibleBanner, voicePrompt } from "@/lib/cockpit-flow";
import type { PilotState } from "@/shared/hud/cockpit-frames";

const base: ChipFacts = {
  owner: "pilot",
  ready: true,
  firstBlockMsg: null,
  setupNeeded: false,
  profile: "normal",
  skillLabel: null,
  modeDetail: null,
  distM: null,
  homeDistM: null,
  setupStep: null,
  missionState: null,
  seq: null,
  total: null,
  etaS: null,
  landingSeq: false,
  completeAgeMs: null,
  vtolTransitioning: false,
  fcMode: null,
  fcModeName: null,
  family: "copter",
  controlMode: "ados",
  holdReason: null,
  avoidState: null,
  ageMs: 100,
};

describe("flight-state chip (flight-state.json rules)", () => {
  it("reads PILOT · <PROFILE> in blue with the control mode below", () => {
    const c = flightStateChip(base);
    expect(c.text).toBe("PILOT · NORMAL");
    expect(c.color).toBe("#2979FF");
    expect(c.secondary.map((l) => l.text)).toContain("ADOS PILOT");
  });

  it("dims to LINK when the pilot state is older than 2 s", () => {
    expect(flightStateChip({ ...base, ageMs: 2500 })).toMatchObject({ text: "LINK", dim: true });
  });

  it("shows NOT READY with the first blocker and the setup link", () => {
    const c = flightStateChip({ ...base, owner: "idle", ready: false, firstBlockMsg: "Aircraft setup needed", setupNeeded: true });
    expect(c.text).toBe("NOT READY · Aircraft setup needed");
    expect(c.button).toBe("setup");
  });

  it("shows the skill and its detail in cyan", () => {
    expect(flightStateChip({ ...base, owner: "skill", skillLabel: "Follow-Me", modeDetail: "ORBIT 8 m" })).toMatchObject({
      text: "FOLLOW-ME · ORBIT 8 m",
      color: "#22D3EE",
    });
  });

  it("follows the mission states", () => {
    const m = { ...base, owner: "mission_fc", seq: 3, total: 9 };
    expect(flightStateChip({ ...m, missionState: "active" }).text).toBe("MISSION · WP 3/9");
    expect(flightStateChip({ ...m, missionState: "active", landingSeq: true }).text).toBe("MISSION · WP 3/9 · LANDING");
    expect(flightStateChip({ ...m, owner: "fc_mode", fcMode: 5, fcModeName: "LOITER", missionState: "paused" })).toMatchObject({ text: "MISSION PAUSED · WP 3/9", button: "resume" });
    expect(flightStateChip({ ...m, missionState: "complete", completeAgeMs: 2000 }).text).toBe("MISSION COMPLETE");
  });

  it("names RTH, LANDING and other FC modes with Take control", () => {
    expect(flightStateChip({ ...base, owner: "fc_mode", fcMode: 6, fcModeName: "RTL", homeDistM: 42 }).text).toBe("RTH · 42 m");
    expect(flightStateChip({ ...base, owner: "fc_mode", fcMode: 9, fcModeName: "LAND" }).text).toBe("LANDING");
    const loiter = flightStateChip({ ...base, owner: "fc_mode", fcMode: 5, fcModeName: "Loiter" });
    expect(loiter.text).toBe("FC · LOITER · ADOS STANDING BY");
    expect(loiter.button).toBe("take_control");
  });

  it("puts AVOIDING and BLOCKED on the secondary line", () => {
    expect(flightStateChip({ ...base, avoidState: "detour" }).secondary[0].text).toBe("AVOIDING");
    expect(flightStateChip({ ...base, avoidState: "blocked" }).secondary[0].text).toBe("BLOCKED");
  });
});

describe("views and layers", () => {
  it("cycles pilot → map → split → vision", () => {
    expect(["pilot", "map", "split", "vision"].map((v) => nextView(v as "pilot"))).toEqual(["map", "split", "vision", "pilot"]);
  });

  it("applies the levels, keeps leased layers on their own flag, forces vision layers", () => {
    expect([...effectiveLayers("off", {}, "pilot")]).toEqual(["centre_marks"]);
    expect(effectiveLayers("route", {}, "pilot").has("ar_route")).toBe(true);
    expect(effectiveLayers("full", { depth: true }, "pilot").has("depth")).toBe(true);
    expect(effectiveLayers("off", { depth: true }, "pilot").has("depth")).toBe(true);
    const vision = effectiveLayers("off", {}, "vision");
    expect(vision.has("targets") && vision.has("depth") && vision.has("surfaces")).toBe(true);
    expect(effectiveLayers("full", {}, "pilot", ["plugin:x:y"]).has("plugin:x:y")).toBe(true);
  });

  it("draws the flight instruments only at the Instruments level; the centre marks at every level", () => {
    for (const level of LAYER_LEVELS) {
      const on = effectiveLayers(level, {}, "pilot");
      expect(on.has("centre_marks")).toBe(true);
      expect(on.has("instruments")).toBe(level === "instruments");
    }
    expect(effectiveLayers("instruments", { instruments: false }, "pilot").has("instruments")).toBe(false);
    expect(effectiveLayers("full", { instruments: true }, "pilot").has("instruments")).toBe(false);
  });

  it("steps through all four levels both ways and wraps", () => {
    expect(LAYER_LEVELS).toEqual(["instruments", "full", "route", "off"]);
    expect(LAYER_LEVELS.map(nextLevel)).toEqual(["full", "route", "off", "instruments"]);
    expect(LAYER_LEVELS.map(prevLevel)).toEqual(["off", "instruments", "full", "route"]);
  });

  it("draws a plugin overlay layer only at Full and Instruments, and only while its flag is on", () => {
    const id = pluginLayerId("com.example.pod", "reticle");
    expect(id).toBe("plugin:com.example.pod:reticle");
    const at = (level: (typeof LAYER_LEVELS)[number], flags: Record<string, boolean> = {}) =>
      effectiveLayers(level, flags, "pilot", [id]).has(id);
    expect([at("instruments"), at("full"), at("route"), at("off")]).toEqual([true, true, false, false]);
    expect(at("full", { [id]: false })).toBe(false);
  });
});

describe("launch, checklist, profiles", () => {
  it("builds the launch action args", () => {
    expect(launchArgs({ after: "goto", altM: 5, skill: null, goto: { n: 20, e: -5, altM: 5 }, whenDone: "rth" })).toEqual({
      after: "goto",
      alt_m: 5,
      when_done: "rth",
      goto: { n: 20, e: -5, d: -5 },
    });
    expect(launchArgs({ after: "skill", altM: 3, skill: { pluginId: "f", skillId: "follow-me" }, goto: null, whenDone: "none" })).toEqual({
      after: "skill",
      alt_m: 3,
      when_done: null,
      skill: { plugin_id: "f", skill_id: "follow-me" },
    });
  });

  it("refuses the checklist with a failing item, passes with warnings", () => {
    expect(checklistPassable([{ id: "a", label: "", state: "warn" }])).toBe(true);
    expect(checklistPassable([{ id: "a", label: "", state: "fail" }])).toBe(false);
  });

  it("steps the profile and clamps at the ends", () => {
    expect(stepProfile("normal", 1)).toBe("sport");
    expect(stepProfile("sport", 1)).toBe("sport");
    expect(stepProfile(null, -1)).toBe("cine");
  });
});

describe("banners and voice", () => {
  it("resolves banner keys and their enumerated args from setup-strings", () => {
    expect(cockpitText("pilot.banner.ados_engaged", { source: "rc" })).toBe("ADOS engaged by RC switch");
    expect(cockpitText("pilot.banner.fc_control", { mode: "loiter" })).toBe("FC has control · LOITER · PS to take back");
  });

  it("shows a banner for the handover time from first sight", () => {
    const b = { textKey: "pilot.banner.pilot_took_control", args: {}, untilMs: null };
    expect(visibleBanner(b, 1000, 2000)).toBe("Pilot took control");
    expect(visibleBanner(b, 1000, 5000)).toBeNull();
  });

  it("speaks mission changes", () => {
    const p = { mission: { state: "loaded" }, banner: null, rthCountdownS: null, owner: "pilot", fcModeName: null } as unknown as PilotState;
    const q = { ...p, mission: { state: "active" } } as unknown as PilotState;
    expect(voicePrompt(p, q)).toBe("Mission started");
  });
});
