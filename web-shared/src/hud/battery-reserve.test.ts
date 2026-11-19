import { describe, expect, it } from "vitest";

import strings from "../cockpit-spec/setup-strings.json";
import { parseBatteryReserve } from "./pilot-fields";

const S = strings.strings as Record<string, string>;

describe("battery_reserve", () => {
  it("parses need, basis and time home", () => {
    expect(parseBatteryReserve({ need_pct: 52, basis: "energy", return_s: 248 })).toEqual({
      needPct: 52,
      basis: "energy",
      returnS: 248,
    });
    expect(parseBatteryReserve({ need_pct: 30, basis: "floor", return_s: 40 })?.basis).toBe("floor");
  });

  it("is null when absent or malformed", () => {
    expect(parseBatteryReserve(undefined)).toBeNull();
    expect(parseBatteryReserve({ need_pct: 52, basis: "guess", return_s: 248 })).toBeNull();
    expect(parseBatteryReserve({ basis: "energy", return_s: 248 })).toBeNull();
  });

  it("the energy banner and alert name the charge the way home needs", () => {
    expect(S["pilot.banner.low_battery_rth_energy"]).toBe(
      "Battery {pct}% · need {need_pct}% to reach home · Returning in {in_s} s",
    );
    expect(S["alert.low_battery_rth_energy"]).toBe("Low battery: returning home · need {need_pct}% to reach home");
  });
});
