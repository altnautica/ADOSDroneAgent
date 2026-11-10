import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { parsePilotState } from "./cockpit-frames";
import { CHIP_STALE_AFTER_MS } from "./flight-state";
import { flyHereOffered, gotoAdvertised } from "./fly-here-offer";

const FIXTURES = path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures");
const pilot = parsePilotState(JSON.parse(readFileSync(path.join(FIXTURES, "pilot-frame.json"), "utf8")))!;

describe("fly here offer", () => {
  it("is offered only under ados control, when the pilot lists goto and the drone flies", () => {
    expect(flyHereOffered(pilot, 100)).toBe(true);
    for (const owner of ["mission_fc", "skill", "fc_mode", "pilot"] as const) {
      expect(flyHereOffered({ ...pilot, owner }, 100)).toBe(true);
    }
    expect(flyHereOffered({ ...pilot, controlMode: "fc_manual" }, 100)).toBe(false);
    expect(flyHereOffered({ ...pilot, controlMode: "elrs" }, 100)).toBe(false);
    expect(flyHereOffered({ ...pilot, surfaceOps: [] }, 100)).toBe(false);
    expect(flyHereOffered({ ...pilot, landed: true }, 100)).toBe(false);
    expect(flyHereOffered({ ...pilot, armed: false }, 100)).toBe(false);
    expect(flyHereOffered(null, 0)).toBe(false);
  });

  it("is not offered from a stale pilot state", () => {
    expect(flyHereOffered(pilot, CHIP_STALE_AFTER_MS - 1)).toBe(true);
    expect(flyHereOffered(pilot, CHIP_STALE_AFTER_MS)).toBe(false);
  });

  it("reports whether the agent advertises goto at all", () => {
    expect(gotoAdvertised(pilot)).toBe(true);
    expect(gotoAdvertised({ ...pilot, armed: false, landed: true })).toBe(true);
    expect(gotoAdvertised({ ...pilot, surfaceOps: [] })).toBe(false);
    expect(gotoAdvertised(null)).toBe(false);
  });
});
