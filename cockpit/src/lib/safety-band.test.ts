import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import * as band from "@/lib/safety-band";
import { fmtClock } from "@/lib/safety-band";

describe("the flight clock format", () => {
  it("reads m:ss, h:mm:ss past an hour, and a dash for unknown", () => {
    expect(fmtClock(240)).toBe("4:00");
    expect(fmtClock(3725)).toBe("1:02:05");
    expect(fmtClock(null)).toBe("—");
    expect(fmtClock(-1)).toBe("—");
    expect(fmtClock(Number.NaN)).toBe("—");
  });

  it("is all the module carries: the old band's label helpers are gone", () => {
    expect(Object.keys(band)).toEqual(["fmtClock"]);
    const src = readFileSync(path.resolve(__dirname, "safety-band.ts"), "utf8");
    for (const name of ["reachBadge", "armLabel", "linkLabel", "videoLabel", "batteryLabel"]) expect(src, name).not.toContain(name);
  });
});
