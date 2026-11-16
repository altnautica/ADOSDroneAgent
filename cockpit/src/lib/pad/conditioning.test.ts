// The cockpit's stick conditioning against the shared golden vectors
// (`cockpit-spec/stick-golden.json`): the same raw stick positions give the
// same pilot axes on the pad, the on-screen sticks and every other surface.

import { describe, expect, it } from "vitest";

import { pilotAxes, radialDeadzone } from "@/lib/pad/conditioning";
import ds4 from "@/shared/cockpit-spec/ds4.json";
import golden from "@/shared/cockpit-spec/stick-golden.json";

describe("stick golden vectors", () => {
  it("uses the spec's default deadzone", () => {
    expect(golden.deadzone).toBe(ds4.conditioning.radial_deadzone);
  });

  it.each(golden.cases.map((c) => [`${c.mode} ${c.name}`, c] as const))("%s", (_, c) => {
    const dz = "deadzone" in c && typeof c.deadzone === "number" ? c.deadzone : golden.deadzone;
    const sticks = {
      l: radialDeadzone(c.raw.lx, c.raw.ly, dz),
      r: radialDeadzone(c.raw.rx, c.raw.ry, dz),
    };
    const got = pilotAxes(sticks, c.mode as "mode1" | "mode2");
    got.forEach((v, i) => expect(Math.abs(v - c.out[i])).toBeLessThanOrEqual(golden.tolerance));
  });
});
