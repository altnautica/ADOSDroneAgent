import { describe, expect, it } from "vitest";

import { BUILTIN_SKILLS } from "@/lib/skills";
import { STOP_HOLD_MS } from "@/lib/pad/ds4-map";

import { PANEL_LONG_PRESS_MS, eventToCommand, panelLongPressCompletes } from "./use-buttons";

describe("eventToCommand", () => {
  it("maps a bound button", () => {
    expect(eventToCommand({ button: "17", label: "b3", kind: "short", action: "cycle_screen" })).toBe(
      "activate",
    );
  });

  it("leaves a passthrough button to plugins", () => {
    expect(eventToCommand({ button: "5", label: "b1", kind: "short", action: "passthrough" })).toBeNull();
    expect(eventToCommand({ button: "5", label: "b1", kind: "long", action: "passthrough" })).toBeNull();
  });

  it("steps with the encoder and fires or goes back with its switch", () => {
    const enc = (label: string, kind: string) =>
      eventToCommand({ button: "130", label, kind, action: "cycle_screen" });
    expect(enc("enc_cw", "short")).toBe("next");
    expect(enc("ENC_CCW", "short")).toBe("prev");
    expect(enc("enc_sw", "short")).toBe("activate");
    expect(enc("enc_sw", "long")).toBe("back");
    expect(enc("enc_sw", "cancel")).toBeNull();
  });
});

describe("panel long-press as a completed hold", () => {
  it("completes a hold no longer than the long press", () => {
    expect(panelLongPressCompletes(1500)).toBe(true);
    expect(panelLongPressCompletes(PANEL_LONG_PRESS_MS)).toBe(true);
  });

  it("never completes the emergency stop", () => {
    const kill = BUILTIN_SKILLS.find((s) => s.id === "kill")!;
    expect(kill.confirm.ms).toBe(STOP_HOLD_MS);
    expect(panelLongPressCompletes(kill.confirm.ms)).toBe(false);
  });
});
