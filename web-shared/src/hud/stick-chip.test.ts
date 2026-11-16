// The flight-state chip's stick facts: the profile with its horizontal limit,
// the vehicle's horizontal and vertical speed while the sticks fly, a stale
// stick input, and the obstacle clearance coloured by the margin.

import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import tokensJson from "../cockpit-spec/tokens.json";
import { chipInputs, type ChipContext } from "./chip-inputs";
import { parsePilotState } from "./cockpit-frames";
import { flightStateChip, type ChipView } from "./flight-state";
import { specText } from "./spec-text";

const limit = (v: string) => specText("chip.stick.limit", { v });
const obstacle = (m: string) => specText("chip.stick.clearance", { m });
const OBSTACLE_WORD = specText("chip.stick.clearance", { m: "" }).split(" ")[0];
const INPUT_WORD = specText("chip.stick.input_age", { ms: "" }).split(" ")[0];

const FIXTURES = path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures");
const frame = JSON.parse(readFileSync(path.join(FIXTURES, "pilot-frame.json"), "utf8")) as Record<string, unknown>;

const LIMITS = {
  profile: "normal", v_h_mps: 7, v_up_mps: 2.5, v_down_mps: 1.5, yaw_dps: 90,
  a_acc_mps2: 2, a_brk_mps2: 2.5, jerk_mps3: 5, a_v_mps2: 1, yaw_dps2: 180,
  expo_h: 0.25, expo_v: 0.3, expo_yaw: 0.2, stop_m: 12.6, margin_m: 1.5,
};
const STICK = {
  ask_ne_mps: [3, 0], ask_vz_mps: 0, ask_yaw_dps: 0, v_ne_mps: [2.5, 0], a_ne_mps2: [1, 0],
  vz_mps: 0, yaw_rate_dps: 0, braking: false, input_age_ms: 40, input_seq: 812,
};
const SOLE = {
  owner: "pilot", skill: null, avoid_state: "clear", hold_reason: null, min_clearance_m: null,
  route_check: null, place_class: "open", mission: null, sequence: null,
};

const ctx = (extra: Partial<ChipContext> = {}): ChipContext => ({
  homeDistM: null, localPos: null, arFrame: null, arAgeMs: 0, modeDetail: null, completeAgeMs: null, speed: null,
  ...extra,
});

function chip(wire: Record<string, unknown>, extra: Partial<ChipContext> = {}): ChipView {
  const p = parsePilotState({ ...frame, ...SOLE, ...wire });
  if (!p) throw new Error("pilot frame does not parse");
  return flightStateChip(chipInputs(p, 1000, 1000, ctx(extra)));
}

const texts = (c: ChipView) => c.secondary.map((l) => l.text);
const STATE = tokensJson.state as Record<string, string>;

describe("stick limits and command on the pilot frame", () => {
  it("parses profile_limits and stick", () => {
    const p = parsePilotState({ ...frame, profile_limits: LIMITS, stick: STICK })!;
    expect(p.profileLimits).toMatchObject({ profile: "normal", vHMps: 7, aBrkMps2: 2.5, stopM: 12.6, marginM: 1.5 });
    expect(p.stick).toMatchObject({ askNeMps: [3, 0], vNeMps: [2.5, 0], braking: false, inputAgeMs: 40, inputSeq: 812 });
    const bare = parsePilotState(frame)!;
    expect(bare.profileLimits).toBeNull();
    expect(bare.stick).toBeNull();
  });
});

describe("the chip while the sticks fly", () => {
  it("names the profile with its horizontal limit", () => {
    expect(chip({ profile_limits: LIMITS }).text).toBe(`Normal · ${limit("7")}`);
    expect(chip({ profile: "sport", profile_limits: { ...LIMITS, profile: "sport", v_h_mps: 10 } }).text).toBe(`Sport · ${limit("10")}`);
    // A capped limit reads with its decimal.
    expect(chip({ profile_limits: { ...LIMITS, v_h_mps: 2.5 } }).text).toBe(`Normal · ${limit("2.5")}`);
    // An agent without the limits shows the profile alone.
    expect(chip({}).text).toBe("Normal");
  });

  it("shows horizontal and vertical speed while the sticks own the vehicle", () => {
    const flying = texts(chip({ profile_limits: LIMITS }, { speed: { hMps: 3.24, vMps: -0.4 } }));
    expect(flying).toContain(specText("chip.stick.h_speed", { v: "3.2" }));
    expect(flying).toContain(specText("chip.stick.v_speed", { v: "-0.4" }));
    expect(texts(chip({ owner: "skill" }, { speed: { hMps: 3.24, vMps: -0.4 } }))).not.toContain(specText("chip.stick.h_speed", { v: "3.2" }));
  });

  it("shows the stick input age only past 100 ms", () => {
    const late = chip({ stick: { ...STICK, input_age_ms: 150 } });
    const line = late.secondary.find((l) => l.text === specText("chip.stick.input_age", { ms: 150 }));
    expect(line?.color).toBe(STATE.avoiding);
    expect(texts(chip({ stick: { ...STICK, input_age_ms: 100 } })).join()).not.toContain(INPUT_WORD);
    expect(texts(chip({})).join()).not.toContain(INPUT_WORD);
  });
});

describe("the clearance warning", () => {
  it("is yellow under twice the margin and red inside it", () => {
    const near = chip({ profile_limits: LIMITS, min_clearance_m: 2.5 }).secondary.find((l) => l.text === obstacle("2.5"));
    expect(near?.color).toBe(STATE.avoiding);
    const inside = chip({ profile_limits: LIMITS, min_clearance_m: 1.2 }).secondary.find((l) => l.text === obstacle("1.2"));
    expect(inside?.color).toBe(STATE.blocked);
    expect(texts(chip({ profile_limits: LIMITS, min_clearance_m: 3.2 })).join()).not.toContain(OBSTACLE_WORD);
  });

  it("shows under any owner, and only with a known margin", () => {
    expect(texts(chip({ owner: "skill", profile_limits: LIMITS, min_clearance_m: 2 }))).toContain(obstacle("2.0"));
    expect(texts(chip({ min_clearance_m: 0.4 })).join()).not.toContain(OBSTACLE_WORD);
  });
});
