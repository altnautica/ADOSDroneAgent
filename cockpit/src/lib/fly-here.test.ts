import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { enToLatLon, flyHereArgs, flyHereOffered, flyHereRefusal, unprojectEN } from "@/lib/fly-here";
import { computeMapView, latLonToEN, projectEN } from "@/lib/minimap-geometry";
import { parsePilotState } from "@/shared/hud/cockpit-frames";

const FIXTURES = path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures");
const pilot = parsePilotState(JSON.parse(readFileSync(path.join(FIXTURES, "pilot-frame.json"), "utf8")))!;

describe("fly here", () => {
  it("is offered only when the pilot lists goto and the drone flies", () => {
    expect(flyHereOffered(pilot)).toBe(true);
    expect(flyHereOffered({ ...pilot, surfaceOps: [] })).toBe(false);
    expect(flyHereOffered({ ...pilot, landed: true })).toBe(false);
    expect(flyHereOffered({ ...pilot, armed: false })).toBe(false);
    expect(flyHereOffered(null)).toBe(false);
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
  });
});
