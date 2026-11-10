import { describe, expect, it } from "vitest";

import { pilotRefusal } from "@/lib/pilot-refusal";

describe("pilot refusal text", () => {
  it("names the Pause refusals in words", () => {
    expect(pilotRefusal("no_position")).toBe("no position from the flight controller");
    expect(pilotRefusal("failsafe")).toMatch(/failsafe return or landing/);
    expect(pilotRefusal("landed")).toBe("on the ground");
  });

  it("never shows a built-in refusal code raw", () => {
    for (const code of ["disarmed", "not_armed", "busy", "tuning", "nothing_paused", "not_landed", "vehicle_class", "no_fc", "unknown_action"]) {
      expect(pilotRefusal(code)).not.toBe(code);
      expect(pilotRefusal(code)).not.toMatch(/_/);
    }
  });

  it("keeps an unknown code and says rejected for none", () => {
    expect(pilotRefusal("something_new")).toBe("something_new");
    expect(pilotRefusal(undefined)).toBe("rejected");
  });
});
