import { describe, expect, it } from "vitest";

import strings from "@/shared/cockpit-spec/setup-strings.json";
import { stopHoldText, stopSheetBody, stopSheetTitle } from "@/lib/stop-sheet";
import { useConfirmStore } from "@/stores/confirm-store";
import { BUILTIN_SKILL_BY_ID } from "@/lib/skills";

const S = strings.strings as Record<string, string>;

describe("the STOP sheet", () => {
  it("reads its title, body and hold label from the spec", () => {
    expect(stopSheetTitle()).toBe(S["cockpit.estop"]);
    expect(stopSheetTitle()).toBe("Emergency stop");
    expect(stopSheetBody()).toBe(S["confirm.kill.message"]);
    expect(stopHoldText(false, 3000, 0)).toBe(S["stop.hold_label"]);
  });

  it("counts down the time still to hold while held", () => {
    expect(stopHoldText(true, 3000, 0)).toBe("Keep holding · 3.0 s");
    expect(stopHoldText(true, 3000, 1 / 3)).toBe("Keep holding · 2.0 s");
    expect(stopHoldText(true, 3000, 1.2)).toBe("Keep holding · 0.0 s");
  });

  it("has no second confirm step: the pending kill carries no stage", () => {
    useConfirmStore.getState().open(BUILTIN_SKILL_BY_ID.kill);
    const pending = useConfirmStore.getState().pending;
    expect(pending).not.toBeNull();
    expect(Object.keys(pending!)).not.toContain("stage");
    expect("toConfirmStage" in useConfirmStore.getState()).toBe(false);
    useConfirmStore.getState().cancel();
  });
});
