import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { mavFixWord, mavHdop } from "./mav-gps";

describe("MAVLink GPS display", () => {
  it("maps GPS_FIX_TYPE to the fix words", () => {
    expect([0, 1, 2, 3, 4, 5, 6, 7, 8, 42].map(mavFixWord)).toEqual([
      "none", "none", "2d", "3d", "dgps", "rtk_float", "rtk_fixed", "3d", "3d", "none",
    ]);
    expect(mavFixWord(null)).toBeNull();
  });

  it("drops the HDOP unknown sentinel", () => {
    expect(mavHdop(655.35)).toBeNull();
    expect(mavHdop(0.9)).toBe(0.9);
    expect(mavHdop(undefined)).toBeNull();
  });

  it("the Params sensors panel uses both instead of the raw values", () => {
    const s = readFileSync(path.resolve(__dirname, "../components/screens/params-screen.tsx"), "utf8");
    expect(s).not.toContain("String(v.gps.fix_type)");
    expect(s).toContain("mavHdop(");
  });

  it("the GNSS screen labels its fix, RTK and base state words", () => {
    const s = readFileSync(path.resolve(__dirname, "../components/screens/gnss-screen.tsx"), "utf8");
    expect(s).not.toMatch(/value=\{base\.state\}/);
    expect(s).not.toMatch(/fix: r\.fix,/);
  });
});
