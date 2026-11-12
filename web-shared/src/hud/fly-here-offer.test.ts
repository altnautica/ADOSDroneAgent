import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { parsePilotState } from "./cockpit-frames";
import flyHereCases from "./fixtures/fly-here-cases.json";
import { CHIP_STALE_AFTER_MS } from "./flight-state";
import { flyHereOffered, gotoAdvertised, type FlyHereReason } from "./fly-here-offer";
import { hasSpecText, specText } from "./spec-text";

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

  it("says why it is not offered", () => {
    const reason = (...a: Parameters<typeof flyHereOffered>) => flyHereOffered(...a).reason;
    expect(reason(pilot, 100)).toBeUndefined();
    expect(reason(pilot, CHIP_STALE_AFTER_MS)).toBe("pilot_stale");
    expect(reason({ ...pilot, fcFailsafe: true }, 100)).toBe("failsafe");
    expect(reason({ ...pilot, landed: true }, 100)).toBe("landed");
    expect(reason({ ...pilot, armed: false }, 100)).toBe("landed");
    expect(reason({ ...pilot, controlMode: "fc_manual" }, 100)).toBe("control_mode");
    expect(reason({ ...pilot, controlMode: "fc_manual" }, 100, { ...guided, guided: false })).toBe("not_guided");
    expect(reason(null, 0, { ...guided, guided: false })).toBe("not_guided");
    expect(reason(null, 0, { ...guided, landed: true })).toBe("landed");
    expect(reason(null, 0)).toBeUndefined();
    // An unknown arm state is possibly airborne: never "landed". A fresh pilot
    // with no arm state has no flight controller link, and says so.
    expect(flyHereOffered({ ...pilot, armed: null, landed: null }, 100)).toEqual({ route: null, reason: "no_fc_link" });
    expect(reason({ ...pilot, armed: null, landed: null, controlMode: "fc_manual" }, 100)).toBe("no_fc_link");
    expect(reason({ ...pilot, armed: null, landed: null }, 100, { ...guided, guided: false })).toBe("not_guided");
  });

  it("words every reason by a refusal key", () => {
    expect(specText("refusal.not_guided")).toBe("Fly here needs GUIDED on the flight controller");
    expect(specText("refusal.pilot_stale")).toBe("Pilot not answering");
    for (const r of ["failsafe", "landed", "pilot_stale", "control_mode", "not_guided", "no_fc_link"] satisfies FlyHereReason[]) {
      expect(hasSpecText(`refusal.${r}`), r).toBe(true);
    }
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
