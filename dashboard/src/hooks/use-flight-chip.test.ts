import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { navStatusChip } from "@/hooks/use-flight-chip";

const FIXTURES = path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures");
const pilot = JSON.parse(readFileSync(path.join(FIXTURES, "pilot-frame.json"), "utf8")) as unknown;

describe("dashboard flight-state chip", () => {
  it("reads the nav-status pilot entry through the cockpit chip rules", () => {
    const chip = navStatusChip({ pilot: { age_ms: 100, data: pilot } });
    expect(chip).not.toBeNull();
    expect(chip!.text).toContain("FOLLOW");
    expect(chip!.dim).toBe(false);
  });

  it("dims a stale pilot entry and is absent without one", () => {
    expect(navStatusChip({ pilot: { age_ms: 60_000, data: pilot } })!.ruleId).toBe("link_stale");
    expect(navStatusChip({})).toBeNull();
    expect(navStatusChip(undefined)).toBeNull();
  });
});
