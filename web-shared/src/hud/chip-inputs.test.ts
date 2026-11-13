import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import flightStateJson from "../cockpit-spec/flight-state.json";
import tokensJson from "../cockpit-spec/tokens.json";
import chipCases from "./fixtures/chip-cases.json";
import { aglMarkText, bottomRowAgl, chipInputs, goalDistanceM, nextWaypointEtaS } from "./chip-inputs";
import { parseArFrame, parsePilotState, type ArFrame } from "./cockpit-frames";
import { chipFacts, flightStateChip } from "./flight-state";
import { specText } from "./spec-text";

const FIXTURES = path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures");
const fixture = (name: string): unknown => JSON.parse(readFileSync(path.join(FIXTURES, name), "utf8"));

const ar = parseArFrame(fixture("ar-frame.json"))!;
const pilot = parsePilotState(fixture("pilot-frame.json"))!;
const base = { homeDistM: null, modeDetail: null, completeAgeMs: null };
const ctx = { homeDistM: null, localPos: null, arFrame: null, arAgeMs: 0, modeDetail: null, completeAgeMs: null };
const wire = (extra: Record<string, unknown>) => parsePilotState({ ...(fixture("pilot-frame.json") as object), ...extra })!;

describe("chip ETA from the ar frame", () => {
  it("reads the next waypoint's ETA from a fresh frame", () => {
    expect(nextWaypointEtaS(ar, 100)).toBe(42);
    expect(chipFacts(pilot, 0, { ...base, etaS: nextWaypointEtaS(ar, 100) }).etaS).toBe(42);
  });

  it("drops the ETA once the frame is older than 2 s or absent", () => {
    expect(nextWaypointEtaS(ar, 2001)).toBeNull();
    expect(nextWaypointEtaS(null, 0)).toBeNull();
  });
});

describe("chip GOTO distance", () => {
  it("measures the local position to the goal horizontally", () => {
    // Goal (12, -3.5) from (9, 0.5): a 3-4-5 triangle.
    const d = goalDistanceM({ x: 9, y: 0.5 }, pilot.goalNed);
    expect(d).toBeCloseTo(5, 6);
    expect(chipFacts(pilot, 0, { ...base, distM: d }).distM).toBeCloseTo(5, 6);
    const goto = wire({ owner: "goto", mission: null });
    expect(flightStateChip(chipFacts(goto, 0, { ...base, distM: d })).text).toBe("Goto · 5 m");
  });

  it("is null when either end is missing", () => {
    expect(goalDistanceM(null, pilot.goalNed)).toBeNull();
    expect(goalDistanceM({ x: 0, y: 0 }, null)).toBeNull();
    expect(chipFacts(pilot, 0, base).distM).toBeNull();
  });
});

describe("chip inputs", () => {
  it("builds the facts from the pilot state and what sits beside it", () => {
    const f = chipInputs(pilot, 10_500, 10_000, { ...ctx, homeDistM: 30, localPos: { x: 9, y: 0.5 }, arFrame: ar, arAgeMs: 100, modeDetail: "Orbit 8 m" });
    expect(f.ageMs).toBe(500);
    expect(f.distM).toBeCloseTo(5, 6);
    expect(f.etaS).toBe(42);
    expect(f.homeDistM).toBe(30);
    expect(f.modeDetail).toBe("Orbit 8 m");
    expect(flightStateChip(f).text).toBe("Follow · Orbit 8 m");
  });

  it("uses the skill detail only while a skill owns", () => {
    expect(chipInputs(wire({ owner: "pilot" }), 0, 0, { ...ctx, modeDetail: "Orbit 8 m" }).modeDetail).toBeNull();
  });

  it("drives the Transition chip from the pilot kind", () => {
    const p = wire({ vtol_transitioning: true, vehicle_class: "vtol", owner: "fc_mode", mission: null });
    expect(chipInputs(p, 0, 0, ctx).vtolTransitioning).toBe(true);
    expect(flightStateChip(chipInputs(p, 0, 0, ctx)).text).toBe("Transition");
    expect(chipInputs(wire({}), 0, 0, ctx).vtolTransitioning).toBe(false);
  });

  it("names the setup step of a tuning flight, and reads Tuning without one", () => {
    expect(flightStateChip(chipInputs(wire({ owner: "tuning", setup_step: "autotune_roll" }), 0, 0, ctx)).text).toBe("Tuning · Autotune roll");
    expect(flightStateChip(chipInputs(wire({ owner: "tuning" }), 0, 0, ctx)).text).toBe("Tuning");
  });
});

describe("bottom row AGL", () => {
  it("reads the pilot kind's agl_m, falling back to the follow kind", () => {
    const rng = { ...pilot, aglM: 4.2, aglSource: "rangefinder" as const };
    expect(bottomRowAgl(rng, { aglM: 9, aglSource: "rng" })).toEqual({ m: 4.2, mark: null });
    expect(bottomRowAgl({ ...pilot, aglM: null }, { aglM: 9, aglSource: "rng" })).toEqual({ m: 9, mark: null });
    expect(bottomRowAgl(null, null)).toEqual({ m: null, mark: null });
  });

  it("marks a height above home as HOME and a barometric one as BARO, never as a measured AGL", () => {
    // Above the rangefinder's 8 m the pilot kind switches to the EKF height
    // above home: the same value, now marked.
    const below = bottomRowAgl({ ...pilot, aglM: 7.9, aglSource: "rangefinder" }, null);
    const above = bottomRowAgl({ ...pilot, aglM: 8.4, aglSource: "home" }, null);
    expect(below).toEqual({ m: 7.9, mark: null });
    expect(above).toEqual({ m: 8.4, mark: "HOME" });
    expect(bottomRowAgl({ ...pilot, aglM: null }, { aglM: 12, aglSource: "baro" })).toEqual({ m: 12, mark: "BARO" });
  });

  it("reads the marks from the spec", () => {
    expect(aglMarkText("HOME")).toBe("HOME");
    expect(aglMarkText("BARO")).toBe("BARO");
    expect(aglMarkText(null)).toBeNull();
  });
});

describe("chip golden cases (fixtures/chip-cases.json)", () => {
  const colors = tokensJson.state as Record<string, string>;

  it("covers every chip rule", () => {
    const covered = new Set(chipCases.map((c) => c.expected.rule));
    expect(flightStateJson.rules.filter((r) => !covered.has(r.id)).map((r) => r.id)).toEqual([]);
    expect(chipCases.length).toBeGreaterThanOrEqual(30);
  });

  it("writes the launching, failsafe and paused-mission texts as the spec strings do", () => {
    const rule = flightStateJson.rules.find((r) => r.id === "launching")!;
    expect(rule.text).toBe(specText("chip.taking_off", { step: "{step}" }));
    expect(flightStateJson.secondary.fc_failsafe.text).toBe(specText("chip.failsafe"));
    expect(flightStateJson.secondary.mission_paused_line.text).toBe(specText("chip.mission_paused_line", { seq: "{seq}", total: "{total}" }));
  });

  it("reads a mission ETA of 119.6 s as 2:00, never 1:60", () => {
    const facts = chipFacts(wire({ owner: "mission_fc", mission: { state: "active", seq: 4, total: 6 } }), 100, { ...base, etaS: 119.6 });
    expect(flightStateChip(facts).text).toBe("Mission · WP 4/6 · ETA 2:00");
  });

  for (const c of chipCases) {
    it(c.name, () => {
      const i = c.input;
      const p = parsePilotState(i.pilot);
      expect(p).not.toBeNull();
      // The ETA reaches the chip through the next waypoint of an `ar` frame.
      const arFrame = i.ar_eta_s === null ? null : ({ cameras: { c: { items: [{ t: "wp", role: "next", etaS: i.ar_eta_s }] } } } as unknown as ArFrame);
      const v = flightStateChip(
        chipInputs(p!, i.pilot_age_ms, 0, {
          homeDistM: i.home_dist_m,
          localPos: i.local_pos,
          arFrame,
          arAgeMs: 0,
          modeDetail: i.mode_detail,
          completeAgeMs: i.complete_age_ms,
        }),
      );
      expect({
        rule: v.ruleId,
        text: v.text,
        color: v.color,
        dim: v.dim,
        button: v.button,
        secondary: v.secondary.map((s) => s.text),
      }).toEqual({ ...c.expected, color: colors[c.expected.color] });
    });
  }
});
