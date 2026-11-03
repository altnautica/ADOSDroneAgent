import { describe, expect, it } from "vitest";

import { settleGate } from "@/lib/settings/apply-form";
import { stepLeafChange } from "@/lib/settings/guarded-leaf";
import { SELF_HEAL_KEYS, selfHealNeedsGuard } from "@/lib/settings/self-heal";

describe("self-heal guard", () => {
  it("guards only enabling the USB power-cycle", () => {
    expect(selfHealNeedsGuard(SELF_HEAL_KEYS.powerCycle, true)).toBe(true);
    expect(selfHealNeedsGuard(SELF_HEAL_KEYS.powerCycle, false)).toBe(false);
    expect(selfHealNeedsGuard(SELF_HEAL_KEYS.usbEnabled, true)).toBe(false);
    expect(selfHealNeedsGuard(SELF_HEAL_KEYS.wifiEnabled, true)).toBe(false);
    expect(selfHealNeedsGuard(SELF_HEAL_KEYS.failThreshold, 3)).toBe(false);
  });

  it("does not write the power-cycle enable until the guard fires", () => {
    const change = { key: SELF_HEAL_KEYS.powerCycle, value: true };
    const step = stepLeafChange(change, selfHealNeedsGuard(change.key, change.value));
    expect(step.kind).toBe("hold");
    if (step.kind !== "hold") return;
    expect(settleGate(step.gate, false)).toBeNull();
    expect(settleGate(step.gate, true)).toEqual(change);
  });

  it("writes the power-cycle disable at once", () => {
    const change = { key: SELF_HEAL_KEYS.powerCycle, value: false };
    expect(stepLeafChange(change, selfHealNeedsGuard(change.key, change.value))).toEqual({ kind: "write", change });
  });
});
