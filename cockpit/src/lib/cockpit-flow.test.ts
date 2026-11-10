import { describe, expect, it } from "vitest";

import { holdMs, LAUNCH, LAUNCH_STEPS, launchRunning, launchStepText, mavResultText, modeResultText } from "@/lib/cockpit-flow";
import layoutJson from "@/shared/cockpit-spec/layout.json";

describe("confirm holds", () => {
  it("read every hold from the spec's holds_ms", () => {
    expect(holdMs("take_control")).toBe(layoutJson.holds_ms.take_control);
    expect(holdMs("hand_to_fc")).toBe(layoutJson.holds_ms.hand_to_fc);
    expect(holdMs("vtol_transition")).toBe(layoutJson.holds_ms.vtol_transition);
    expect(holdMs("fly_here")).toBe(layoutJson.holds_ms.fly_here);
    expect(holdMs("stop")).toBe(layoutJson.holds_ms.stop);
  });
});

describe("launch values", () => {
  it("take the altitude range and default from the spec, with no default go-to point", () => {
    expect(LAUNCH).toEqual({
      altMinM: layoutJson.launch.alt_min_m,
      altMaxM: layoutJson.launch.alt_max_m,
      altDefaultM: layoutJson.launch.alt_default_m,
      gotoDefault: null,
    });
  });
});

describe("launch sequence", () => {
  it("runs until done or an error", () => {
    expect(launchRunning(null)).toBe(false);
    expect(launchRunning({ step: "takeoff", after: "hold", error: null })).toBe(true);
    expect(launchRunning({ step: "checks", after: null, error: null })).toBe(true);
    expect(launchRunning({ step: "done", after: "hold", error: null })).toBe(false);
    expect(launchRunning({ step: "arm", after: null, error: "arm_refused" })).toBe(false);
  });

  it("names every step from the spec", () => {
    expect(LAUNCH_STEPS.map(launchStepText)).toEqual(["Checks", "Arm", "Take off", "After", "Done"]);
  });
});

describe("mode change results", () => {
  it("names a flight controller's answer, and anything else through the refusal texts", () => {
    expect(mavResultText("DENIED")).toBe("Denied");
    expect(mavResultText("TEMPORARILY_REJECTED")).toBe("Temporarily rejected");
    expect(mavResultText("airborne")).toBe("Already flying");
    expect(mavResultText("not_ready:position")).toMatch(/^Not ready: /);
    expect(mavResultText("WEIRD")).toBe("Refused: WEIRD");
  });

  it("reads done, or the reason", () => {
    expect(modeResultText("Profile", { ok: true })).toBe("Profile: done");
    expect(modeResultText("Profile", { ok: false, reason: "Already flying" })).toBe("Profile: Already flying");
    expect(modeResultText("Profile", { ok: false })).toBe("Profile: refused");
  });
});
