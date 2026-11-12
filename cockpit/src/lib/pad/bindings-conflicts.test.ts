import { describe, expect, it } from "vitest";

import { findConflicts } from "@/lib/pad/bindings";
import { STOP_ACTION, type PadBinding } from "@/lib/pad/ds4-map";

const row = (control: string, action: string): PadBinding => ({
  control: control as PadBinding["control"],
  with: [],
  gesture: "hold",
  hold_ms: 1500,
  when: null,
  action,
  rumble: null,
  locked: false,
});

describe("binding conflicts", () => {
  it("lets the emergency stop sit on several controls", () => {
    expect(findConflicts([row("cross", STOP_ACTION), row("circle", STOP_ACTION)])).toEqual([]);
  });

  it("still flags any other action bound to two controls", () => {
    expect(findConflicts([row("cross", "rth"), row("circle", "rth")])).toEqual([
      { rows: [0, 1], reason: "same_action" },
    ]);
  });

  it("still flags two stops on the same slot", () => {
    expect(findConflicts([row("cross", STOP_ACTION), row("cross", STOP_ACTION)])).toEqual([
      { rows: [0, 1], reason: "same_slot" },
    ]);
  });
});
