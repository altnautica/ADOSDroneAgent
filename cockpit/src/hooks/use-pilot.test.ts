import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { chipFacts, goalDistanceM, nextWaypointEtaS } from "@/hooks/use-pilot";
import { parseArFrame, parsePilotState } from "@/shared/hud/cockpit-frames";
import { linkLossLine } from "@/lib/cockpit-flow";
import { flightStateChip } from "@/lib/flight-state";

const FIXTURES = path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures");
const fixture = (name: string): unknown => JSON.parse(readFileSync(path.join(FIXTURES, name), "utf8"));

const ar = parseArFrame(fixture("ar-frame.json"))!;
const pilot = parsePilotState(fixture("pilot-frame.json"))!;
const base = { homeDistM: null, modeDetail: null, completeAgeMs: null };

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
    const goto = parsePilotState({ ...(fixture("pilot-frame.json") as object), owner: "goto", mission: null })!;
    expect(flightStateChip(chipFacts(goto, 0, { ...base, distM: d }))?.text).toBe("GOTO · 5 m");
  });

  it("is null when either end is missing", () => {
    expect(goalDistanceM(null, pilot.goalNed)).toBeNull();
    expect(goalDistanceM({ x: 0, y: 0 }, null)).toBeNull();
    expect(chipFacts(pilot, 0, base).distM).toBeNull();
  });
});

describe("link-loss countdown and VTOL transition", () => {
  const withWire = (extra: Record<string, unknown>) =>
    parsePilotState({ ...(fixture("pilot-frame.json") as object), link_loss_in_s: 4.2, ...extra })!;

  it("names the action the agent reports", () => {
    expect(linkLossLine(withWire({ link_loss_action: "land" }))).toBe("Link lost: Land in 5 s");
    expect(linkLossLine(withWire({ link_loss_action: "hold" }))).toBe("Link lost: Hold in 5 s");
    expect(linkLossLine(withWire({ link_loss_action: "rth" }))).toBe("Link lost: Return home in 5 s");
  });

  it("never guesses the action, and is null with no countdown", () => {
    expect(linkLossLine(withWire({ link_loss_action: undefined }))).toBe("Link lost: — in 5 s");
    expect(linkLossLine(withWire({ link_loss_in_s: null, link_loss_action: "land" }))).toBeNull();
  });

  it("drives the TRANSITION chip from the pilot kind", () => {
    const p = withWire({ vtol_transitioning: true, vehicle_class: "vtol", owner: "fc_mode", mission: null });
    expect(chipFacts(p, 0, base).vtolTransitioning).toBe(true);
    expect(flightStateChip(chipFacts(p, 0, base))?.text).toBe("TRANSITION");
    expect(chipFacts(withWire({}), 0, base).vtolTransitioning).toBe(false);
  });
});
