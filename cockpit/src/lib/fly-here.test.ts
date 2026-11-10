import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { FLY_HERE_TAKES_OVER, enToLatLon, flyHereArgs, flyHereOffered, flyHerePosition, flyHereRefusal, unprojectEN } from "@/lib/fly-here";
import { computeMapView, latLonToEN, projectEN } from "@/lib/minimap-geometry";
import { parsePilotState } from "@/shared/hud/cockpit-frames";

const FIXTURES = path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures");
const pilot = parsePilotState(JSON.parse(readFileSync(path.join(FIXTURES, "pilot-frame.json"), "utf8")))!;

describe("fly here", () => {
  it("is offered only under ados control, when the pilot lists goto and the drone flies", () => {
    expect(flyHereOffered(pilot)).toBe(true);
    for (const owner of ["mission_fc", "skill", "fc_mode", "pilot"] as const) {
      expect(flyHereOffered({ ...pilot, owner } as typeof pilot)).toBe(true);
    }
    expect(flyHereOffered({ ...pilot, controlMode: "fc_manual" })).toBe(false);
    expect(flyHereOffered({ ...pilot, controlMode: "elrs" })).toBe(false);
    expect(flyHereOffered({ ...pilot, surfaceOps: [] })).toBe(false);
    expect(flyHereOffered({ ...pilot, landed: true })).toBe(false);
    expect(flyHereOffered({ ...pilot, armed: false })).toBe(false);
    expect(flyHereOffered(null)).toBe(false);
  });

  it("warns that the hold takes over a running mission or skill", () => {
    expect(FLY_HERE_TAKES_OVER).toMatch(/takes over/i);
    expect(FLY_HERE_TAKES_OVER).toMatch(/mission/);
  });

  it("maps a point on the minimap back to the lat/lon drawn there", () => {
    const ref = { lat: 12.97, lon: 77.59 };
    const target = { lat: 12.9705, lon: 77.5893 };
    const en = latLonToEN(target.lat, target.lon, ref.lat, ref.lon);
    const view = computeMapView([{ e: 0, n: 0 }, en], 100, 12, 40)!;
    const pt = projectEN(en.e, en.n, view);
    const back = unprojectEN(pt.x, pt.y, view);
    const ll = enToLatLon(back.e, back.n, ref);
    expect(ll.lat).toBeCloseTo(target.lat, 6);
    expect(ll.lon).toBeCloseTo(target.lon, 6);
  });

  it("sends the current height above home, never below the floor", () => {
    expect(flyHereArgs({ lat: 1, lon: 2 }, 12.34)).toEqual({ lat: 1, lon: 2, alt_m: 12.3 });
    expect(flyHereArgs({ lat: 1, lon: 2 }, 0.5)?.alt_m).toBe(3);
  });

  it("refuses without a height reading rather than falling back to the floor", () => {
    expect(flyHereArgs({ lat: 1, lon: 2 }, null)).toBeNull();
    expect(flyHereArgs({ lat: 1, lon: 2 }, undefined)).toBeNull();
    expect(flyHereArgs({ lat: 1, lon: 2 }, Number.NaN)).toBeNull();
  });

  it("names a refusal", () => {
    expect(flyHereRefusal("no_home")).toMatch(/home/);
    expect(flyHereRefusal("no response")).toBe("No answer from the drone");
    expect(flyHereRefusal("launching")).toMatch(/climb/);
    expect(flyHereRefusal("control_mode")).toMatch(/flight controller/);
  });

  it("names every refusal the pilot returns, failsafe included", () => {
    expect(flyHereRefusal("failsafe")).toBe(
      "Fly Here refused: the drone is in a failsafe return or landing; change mode first",
    );
    for (const code of ["bad_args", "not_armed", "landed", "tuning", "no_home", "launching", "control_mode", "failsafe"]) {
      expect(flyHereRefusal(code)).not.toMatch(/^Refused/);
      expect(flyHereRefusal(code)).not.toBe(`Refused: ${code}`);
    }
  });

  it("places the drone from the FC position through the EKF origin without a GNSS fix", () => {
    const pos = { lat: 12.97, lon: 77.59 };
    expect(flyHerePosition(pos, 3, true)).toEqual({ drone: pos, gnss: true });
    expect(flyHerePosition(pos, 0, true)).toEqual({ drone: pos, gnss: false });
    expect(flyHerePosition(pos, 1, true)).toEqual({ drone: pos, gnss: false });
    expect(flyHerePosition(pos, null, true)).toEqual({ drone: pos, gnss: true });
  });

  it("has no drone position (so no Fly Here) without a fix and without an EKF origin", () => {
    expect(flyHerePosition({ lat: 0, lon: 0 }, 0, true).drone).toBeNull();
    expect(flyHerePosition(null, 3, true).drone).toBeNull();
    expect(flyHerePosition({ lat: 12.97, lon: 77.59 }, 3, false).drone).toBeNull();
  });
});
