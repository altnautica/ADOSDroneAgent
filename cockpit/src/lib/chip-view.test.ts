import { describe, expect, it, vi } from "vitest";

vi.mock("@/lib/cockpit-actions", () => ({ runCockpitAction: vi.fn() }));
vi.mock("@/stores/cockpit-ui-store", async (orig) => ({ ...(await orig<object>()), abortLaunch: vi.fn() }));

import { chipButton, chipFullText, chipLines } from "@/lib/chip-view";
import { runCockpitAction } from "@/lib/cockpit-actions";
import type { ChipView } from "@/shared/hud/flight-state";
import chipCases from "@/shared/hud/fixtures/chip-cases.json";
import { abortLaunch } from "@/stores/cockpit-ui-store";

interface Expected {
  rule: string;
  text: string;
  dim: boolean;
  button: ChipView["button"];
  secondary: string[];
}

/** The shared golden case's expected chip view. */
function expected(name: string): Expected {
  const cases: readonly { name: string; expected: Expected }[] = chipCases as unknown as { name: string; expected: Expected }[];
  const c = cases.find((x) => x.name === name);
  if (!c) throw new Error(name);
  return c.expected;
}

describe("the flight-state chip", () => {
  it("wires Resume to the pilot resume and Abort to the launch abort", () => {
    chipButton("resume")?.run();
    expect(runCockpitAction).toHaveBeenCalledWith("resume");
    const abort = chipButton("abort");
    expect(abort?.label).toBe("Abort");
    abort?.run();
    expect(abortLaunch).toHaveBeenCalled();
    expect(chipButton(null)).toBeNull();
  });

  it("adds the reduced-rate line while the nav stream rides the radio", () => {
    const chip: ChipView = { ruleId: "x", text: "PILOT · NORMAL", color: "#fff", dim: false, button: null, secondary: [{ text: "ADOS PILOT", color: "#aaa" }] };
    expect(chipLines(chip, false).map((l) => l.text)).toEqual(["ADOS PILOT"]);
    expect(chipLines(chip, true).map((l) => l.text)).toEqual(["ADOS PILOT", "Reduced update rate (radio)"]);
    expect(chipFullText(chip.text, chipLines(chip, true))).toBe("Flight state: PILOT · NORMAL · ADOS PILOT · Reduced update rate (radio)");
  });

  it("renders the shared view's failsafe and paused-mission lines and their buttons", () => {
    for (const [name, label] of [["pilot_holds_paused_mission", "Resume"], ["launching_takeoff", "Abort"], ["fc_failsafe_rtl_reads_failsafe_first", null]] as const) {
      const e = expected(name);
      const chip: ChipView = { ruleId: e.rule, text: e.text, color: "#fff", dim: e.dim, button: e.button, secondary: e.secondary.map((text) => ({ text, color: "#aaa" })) };
      expect(chipButton(chip.button)?.label ?? null, name).toBe(label);
      expect(chipLines(chip, false).map((l) => l.text), name).toEqual(e.secondary);
    }
  });
});
