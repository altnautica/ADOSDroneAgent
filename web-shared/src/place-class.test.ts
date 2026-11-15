import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

import { parsePilotState } from "./hud/cockpit-frames";
import { placeExplainLine, placeMargins } from "./place-class";

const FIXTURES = path.resolve(__dirname, "../../crates/ados-control/tests/fixtures");
const confined = parsePilotState(JSON.parse(readFileSync(path.join(FIXTURES, "pilot-frame-confined.json"), "utf8")))!;
const open = parsePilotState(JSON.parse(readFileSync(path.join(FIXTURES, "pilot-frame.json"), "utf8")))!;

describe("place margins from the agent config", () => {
  it("reads the obstacle margin and the prop-tip radius", () => {
    expect(placeMargins({ pilot: { obstacle_margin_m: 0.3, prop_tip_radius_m: 0.377 } })).toEqual({ obstacleMarginM: 0.3, propTipRadiusM: 0.377 });
  });

  it("reads nulls from a config without them", () => {
    expect(placeMargins({ pilot: { prop_tip_radius_m: null } })).toEqual({ obstacleMarginM: null, propTipRadiusM: null });
    expect(placeMargins(null)).toEqual({ obstacleMarginM: null, propTipRadiusM: null });
  });
});

describe("the picker's explain line", () => {
  const margins = { obstacleMarginM: 0.3, propTipRadiusM: 0.377 };

  it("uses the route check's margin while the pilot is confined", () => {
    expect(placeExplainLine("confined", confined, { obstacleMarginM: null, propTipRadiusM: null })).toContain("0.48 m from walls");
  });

  it("uses the config margin raised to the class floor otherwise", () => {
    expect(placeExplainLine("confined", open, margins)).toContain("0.48 m from walls");
    expect(placeExplainLine("open", confined, margins)).toBe("Open sites: 0.5 m from obstacles.");
    expect(placeExplainLine("open", null, { obstacleMarginM: null, propTipRadiusM: null })).toBe("Open sites: 1.5 m from obstacles.");
  });

  it("shows no margin for a confined place without a prop-tip radius", () => {
    expect(placeExplainLine("confined", open, { obstacleMarginM: 0.6, propTipRadiusM: null })).toContain("— m from walls");
  });
});
