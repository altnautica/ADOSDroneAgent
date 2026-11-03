import { describe, expect, it } from "vitest";

import { settleGate } from "@/lib/settings/apply-form";
import { stepLeafChange } from "@/lib/settings/guarded-leaf";

const change = { key: "a.b", value: true };

describe("stepLeafChange", () => {
  it("writes an unguarded change at once", () => {
    expect(stepLeafChange(change, false)).toEqual({ kind: "write", change });
  });

  it("holds a guarded change and sends nothing until the guard fires", () => {
    const step = stepLeafChange(change, true);
    expect(step.kind).toBe("hold");
    if (step.kind !== "hold") return;
    expect(settleGate(step.gate, false)).toBeNull();
    expect(settleGate(step.gate, true)).toEqual(change);
  });
});
