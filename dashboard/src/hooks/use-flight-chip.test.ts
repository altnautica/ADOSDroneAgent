import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { navStatusChip, type ChipSources } from "@/hooks/use-flight-chip";

const FIXTURES = path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures");
const pilot = JSON.parse(readFileSync(path.join(FIXTURES, "pilot-frame.json"), "utf8")) as Record<string, unknown>;

const base: ChipSources = {
  body: { pilot: { age_ms: 100, data: pilot } },
  receivedAtMs: 10_000,
  nowMs: 10_000,
  telemetry: null,
  modeDetail: null,
  completeAgeMs: null,
};

describe("dashboard flight-state chip", () => {
  it("reads the nav-status pilot entry through the cockpit chip rules", () => {
    const chip = navStatusChip(base);
    expect(chip).not.toBeNull();
    expect(chip!.text).toContain("Follow");
    expect(chip!.dim).toBe(false);
  });

  it("dims a stale pilot entry and is absent without one", () => {
    expect(navStatusChip({ ...base, body: { pilot: { age_ms: 60_000, data: pilot } } })!.ruleId).toBe("link_stale");
    expect(navStatusChip({ ...base, body: {} })).toBeNull();
    expect(navStatusChip({ ...base, body: undefined })).toBeNull();
  });

  it("ages the last frame after a failed read so the chip turns Link, never null", () => {
    // The body is the last one received; reads failed for 5 s since.
    const chip = navStatusChip({ ...base, nowMs: base.receivedAtMs + 5_000 });
    expect(chip).not.toBeNull();
    expect(chip!.ruleId).toBe("link_stale");
  });

  it("feeds the home distance and the skill detail through chipInputs", () => {
    const skillPilot = { ...pilot, owner: "skill" };
    const chip = navStatusChip({
      ...base,
      body: { pilot: { age_ms: 100, data: skillPilot } },
      telemetry: { home_distance: 120, local_position: null },
      modeDetail: "Orbit 8 m",
    });
    expect(JSON.stringify(chip)).toContain("Orbit 8 m");
  });

  it("measures the goal distance only from a local position at most 2 s old", () => {
    const gotoPilot = { ...pilot, owner: "goto" };
    const chip = (age_ms: number | undefined) =>
      navStatusChip({
        ...base,
        body: { pilot: { age_ms: 100, data: gotoPilot } },
        telemetry: { home_distance: null, local_position: { x: 0, y: 0, age_ms } },
      })!.text;
    // goal_ned (12, -3.5) from the origin: 12.5 m, shown whole.
    expect(chip(2_000)).toBe("Goto · 13 m");
    expect(chip(2_001)).not.toContain("13");
    expect(chip(5_000)).not.toContain("13");
    expect(chip(undefined)).not.toContain("13");
  });
});
