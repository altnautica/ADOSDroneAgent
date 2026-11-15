import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

import { parsePilotState, parseRouteCheck, type RouteCheck } from "./cockpit-frames";
import {
  confinedChipText,
  effectiveMarginM,
  placeClassAction,
  placeClassExplain,
  placeClassLabel,
  routeCheckReport,
  speedCapLine,
} from "./route-check";
import { refusalText } from "./refusal";

// The golden confined frame the agent's own tests use, read in place.
const FIXTURES = path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures");
const fixture = (name: string): Record<string, unknown> => JSON.parse(readFileSync(path.join(FIXTURES, name), "utf8"));
const confined = parsePilotState(fixture("pilot-frame-confined.json"))!;
const plain = parsePilotState(fixture("pilot-frame.json"))!;

const check = (over: Partial<RouteCheck> = {}): RouteCheck => ({
  state: "feasible",
  requiredM: 0.577,
  marginM: 0.477,
  sigmaM: 0.02,
  pinch: null,
  unverifiedLegs: [],
  ...over,
});

describe("the confined frame through the parser", () => {
  it("reads the place class, the route check and the confined-near cap", () => {
    expect(confined.placeClass).toBe("confined");
    expect(confined.routeCheck).toEqual({
      state: "infeasible",
      requiredM: 0.577,
      marginM: 0.477,
      sigmaM: 0.02,
      pinch: { fromSeq: 3, toSeq: 4, ned: [41.25, -17.5, -2], clearanceM: 0.52, requiredM: 0.577 },
      unverifiedLegs: [5, 6],
    });
    expect(confined.speedCap).toEqual({ reason: "confined-near", vHMps: 0.5 });
    expect(confined.holdReason).toBe("route-infeasible");
  });

  it("defaults to the open class with no route check", () => {
    expect(plain.placeClass).toBe("open");
    expect(plain.routeCheck).toBeNull();
    expect(parsePilotState({ ...fixture("pilot-frame.json"), place_class: "cave" })!.placeClass).toBe("open");
  });

  it("drops a malformed route check and a malformed pinch", () => {
    expect(parseRouteCheck({ state: "maybe" })).toBeNull();
    expect(parseRouteCheck(null)).toBeNull();
    const noPinch = parseRouteCheck({ state: "unverified", required_m: 0.6, margin_m: 0.5, sigma_m: null, pinch: { from_seq: 1 }, unverified_legs: [2, "x", 3] });
    expect(noPinch).toEqual({ state: "unverified", requiredM: 0.6, marginM: 0.5, sigmaM: null, pinch: null, unverifiedLegs: [2, 3] });
  });
});

describe("the confined chip", () => {
  it("shows Confined only for the confined class", () => {
    expect(confinedChipText(confined)).toBe("Confined");
    expect(confinedChipText(plain)).toBeNull();
  });
});

describe("the route-check report", () => {
  it("reads the golden infeasible check: leg, clearance, required and the pinch position", () => {
    expect(routeCheckReport(confined.routeCheck)).toEqual({
      state: "infeasible",
      severity: "critical",
      title: "Route check",
      line: "Route too tight: leg 3→4 has 0.52 m, needs 0.58 m",
      pinchAt: "At N 41.3 m, E -17.5 m, height 2.0 m",
    });
  });

  it("reads a feasible check with the clearance it keeps", () => {
    const r = routeCheckReport(check({ pinch: { fromSeq: 1, toSeq: 2, ned: [3, 4, -5.04], clearanceM: 1.2, requiredM: 0.577 } }))!;
    expect(r.state).toBe("feasible");
    expect(r.severity).toBe("ok");
    expect(r.line).toBe("Route clear · keeps 0.58 m");
    expect(r.pinchAt).toBe("At N 3.0 m, E 4.0 m, height 5.0 m");
  });

  it("reads an unverified check as a warning without a pinch line when none was observed", () => {
    expect(routeCheckReport(check({ state: "unverified", unverifiedLegs: [2] }))).toEqual({
      state: "unverified",
      severity: "warn",
      title: "Route check",
      line: "Route not seen yet · slow near walls",
      pinchAt: null,
    });
  });

  it("is null with no route check", () => {
    expect(routeCheckReport(null)).toBeNull();
    expect(routeCheckReport(plain.routeCheck)).toBeNull();
  });
});

describe("the speed-cap line", () => {
  it("reads the confined-near cap from its own spec key", () => {
    expect(speedCapLine(confined.speedCap)).toBe("Near the clearance limit: 0.5 m/s max");
  });

  it("keeps the GNSS-denied line and has none for an unknown reason or no cap", () => {
    expect(speedCapLine({ reason: "gnss-denied", vHMps: 2 })).toBe("GNSS-denied: 2 m/s max");
    expect(speedCapLine({ reason: "made-up", vHMps: 2 })).toBeNull();
    expect(speedCapLine(null)).toBeNull();
  });
});

describe("the place-class picker", () => {
  it("labels both classes and names the pilot action", () => {
    expect(placeClassLabel("open")).toBe("Open");
    expect(placeClassLabel("confined")).toBe("Confined space");
    expect(placeClassAction("confined")).toBe("place_class:confined");
    expect(placeClassAction("open")).toBe("place_class:open");
  });

  it("explains each class with the effective margin", () => {
    expect(placeClassExplain("open", 1.5)).toBe("Open sites: 1.5 m from obstacles.");
    expect(placeClassExplain("confined", 0.477)).toBe(
      "Mines, tunnels and rooms: 0.48 m from walls, 0.5 m/s near the limit. Choose it only for a confined place.",
    );
    expect(placeClassExplain("confined", null)).toBe(
      "Mines, tunnels and rooms: — m from walls, 0.5 m/s near the limit. Choose it only for a confined place.",
    );
  });

  it("raises the configured margin to the class floor", () => {
    expect(effectiveMarginM("open", null, null)).toBe(1.5);
    expect(effectiveMarginM("open", 0.3, null)).toBe(0.5);
    expect(effectiveMarginM("confined", 0.3, 0.377)).toBeCloseTo(0.477, 6);
    expect(effectiveMarginM("confined", 1.0, 0.377)).toBe(1.0);
    expect(effectiveMarginM("confined", 0.6, null)).toBeNull();
  });
});

describe("the confined refusals", () => {
  it("read through the refusal table", () => {
    expect(refusalText("route_infeasible")).toBe("Route too tight for the confined profile");
    expect(refusalText("no_prop_radius")).toBe("Set the propeller tip radius for the confined profile");
    expect(refusalText("no_stall")).toBe("No stalled waypoint to skip");
  });
});
