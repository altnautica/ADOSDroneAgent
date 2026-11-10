import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { parsePilotState } from "./cockpit-frames";
import flyHereCases from "./fixtures/fly-here-cases.json";
import { CHIP_STALE_AFTER_MS } from "./flight-state";
import { flyHereOffered, gotoAdvertised } from "./fly-here-offer";

const FIXTURES = path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures");
const pilot = parsePilotState(JSON.parse(readFileSync(path.join(FIXTURES, "pilot-frame.json"), "utf8")))!;
const route = (...a: Parameters<typeof flyHereOffered>) => flyHereOffered(...a).route;
const guided = { guided: true, armed: true, landed: false };

describe("fly here offer", () => {
  it("routes through the pilot under ados control, when it lists goto and the drone flies", () => {
    expect(route(pilot, 100)).toBe("pilot");
    for (const owner of ["mission_fc", "skill", "fc_mode", "pilot"] as const) {
      expect(route({ ...pilot, owner }, 100)).toBe("pilot");
    }
    expect(route({ ...pilot, controlMode: "fc_manual" }, 100)).toBeNull();
    expect(route({ ...pilot, controlMode: "elrs" }, 100)).toBeNull();
    expect(route({ ...pilot, surfaceOps: [] }, 100)).toBeNull();
    expect(route({ ...pilot, landed: true }, 100)).toBeNull();
    expect(route({ ...pilot, armed: false }, 100)).toBeNull();
    expect(route(null, 0)).toBeNull();
  });

  it("is not offered through the pilot during an FC failsafe", () => {
    expect(route({ ...pilot, fcFailsafe: true }, 100)).toBeNull();
    expect(route({ ...pilot, fcFailsafe: true }, 100, { ...guided, guided: false })).toBeNull();
  });

  it("falls back to the FC in GUIDED, armed and flying", () => {
    expect(route({ ...pilot, controlMode: "fc_manual" }, 100, guided)).toBe("fc");
    expect(route(null, 0, guided)).toBe("fc");
    expect(route(null, 0, { ...guided, guided: false })).toBeNull();
    expect(route(null, 0, { ...guided, landed: true })).toBeNull();
    expect(route(null, 0, { ...guided, armed: false })).toBeNull();
    expect(route(pilot, 100, guided)).toBe("pilot");
  });

  it("is not offered from a stale pilot state", () => {
    expect(route(pilot, CHIP_STALE_AFTER_MS - 1)).toBe("pilot");
    expect(route(pilot, CHIP_STALE_AFTER_MS)).toBeNull();
  });

  it("reports whether the agent advertises goto at all", () => {
    expect(gotoAdvertised(pilot)).toBe(true);
    expect(gotoAdvertised({ ...pilot, armed: false, landed: true })).toBe(true);
    expect(gotoAdvertised({ ...pilot, surfaceOps: [] })).toBe(false);
    expect(gotoAdvertised(null)).toBe(false);
  });
});

describe("fly here golden cases (fixtures/fly-here-cases.json)", () => {
  it("has at least ten cases", () => {
    expect(flyHereCases.length).toBeGreaterThanOrEqual(10);
  });

  for (const c of flyHereCases) {
    it(c.name, () => {
      const p = c.input.pilot === null ? null : parsePilotState(c.input.pilot);
      if (c.input.pilot !== null) expect(p).not.toBeNull();
      expect(flyHereOffered(p, c.input.pilot_age_ms, c.input.fc)).toEqual(c.expected);
    });
  }
});
