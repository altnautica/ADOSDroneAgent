import { describe, expect, it } from "vitest";

import { batteryFailsafeFixable } from "./battery-failsafe-fix";
import { specText } from "./spec-text";

const item = (key: string, ok = false) => ({ id: "battery_failsafe", ok, msg: specText(key) });

describe("the battery failsafe fix", () => {
  it("is offered on a disarmed vehicle when an action does nothing", () => {
    for (const k of ["none", "none_confined", "low_none", "low_none_confined", "low_none_plane", "critical_none", "confined_rtl", "differs", "differs_confined", "differs_low"]) {
      expect(batteryFailsafeFixable(item(`readiness.battery_failsafe.${k}`), false), k).toBe(true);
    }
  });

  it("is not offered for unset levels, no monitor or an unread state", () => {
    for (const k of ["levels_unset", "no_monitor", "unread"]) {
      expect(batteryFailsafeFixable(item(`readiness.battery_failsafe.${k}`), false), k).toBe(false);
    }
  });

  it("is not offered while armed or with the arm state unknown", () => {
    expect(batteryFailsafeFixable(item("readiness.battery_failsafe.none"), true)).toBe(false);
    expect(batteryFailsafeFixable(item("readiness.battery_failsafe.none"), null)).toBe(false);
  });

  it("is not offered on a passing item or another item", () => {
    expect(batteryFailsafeFixable(item("readiness.battery_failsafe.none", true), false)).toBe(false);
    expect(batteryFailsafeFixable({ id: "sticks", ok: false, msg: specText("readiness.battery_failsafe.none") }, false)).toBe(false);
  });
});
