import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import {
  avoidOffWarning,
  bannerSeverity,
  holdMs,
  LAUNCH,
  LAUNCH_STEPS,
  launchRunning,
  launchStepText,
  launchStepUpper,
  lowBatteryLine,
  mavResultText,
  modeHoldLabel,
  modeResultText,
  visibleBanner,
} from "@/lib/cockpit-flow";
import layoutJson from "@/shared/cockpit-spec/layout.json";
import { parsePilotState, type PilotState } from "@/shared/hud/cockpit-frames";
import { chipFacts, flightStateChip } from "@/shared/hud/flight-state";

const PILOT_FRAME = JSON.parse(
  readFileSync(path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures/pilot-frame.json"), "utf8"),
) as Record<string, unknown>;

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

describe("launch sequence and the chip", () => {
  const sequences: PilotState["sequence"][] = [
    null,
    { step: "checks", after: null, error: null },
    { step: "takeoff", after: "hold", error: null },
    { step: "done", after: "hold", error: null },
    { step: "arm", after: null, error: "arm_refused" },
    { step: "arm", after: null, error: "" },
  ];
  it("runs exactly when the chip's launching rule does, with the chip's text", () => {
    const base = parsePilotState(PILOT_FRAME)!;
    for (const sequence of sequences) {
      const p = { ...base, sequence };
      const facts = chipFacts(p, 0, { homeDistM: null, modeDetail: null, completeAgeMs: null });
      expect(launchRunning(sequence)).toBe(facts.launchStep !== null);
      if (launchRunning(sequence)) {
        const chip = flightStateChip({ ...facts, owner: "pilot" });
        expect(chip.text).toBe(`Taking off · ${launchStepUpper(sequence.step)}`);
      }
    }
    expect(launchStepUpper("takeoff")).toBe("CLIMB");
  });
});

describe("countdown banners", () => {
  it("take their severity from the spec, both critical", () => {
    expect(bannerSeverity("low_battery_rth")).toBe(layoutJson.banners.severity.low_battery_rth);
    expect(bannerSeverity("link_loss")).toBe(layoutJson.banners.severity.link_loss);
    expect(bannerSeverity("low_battery_rth")).toBe("critical");
    expect(bannerSeverity("link_loss")).toBe("critical");
  });

  it("count whole seconds rounded up", () => {
    expect(lowBatteryLine(9.2)).toBe("Low battery: returning home in 10 s");
    expect(lowBatteryLine(10)).toBe("Low battery: returning home in 10 s");
    expect(lowBatteryLine(0.001)).toBe("Low battery: returning home in 1 s");
    expect(lowBatteryLine(-1)).toBe("Low battery: returning home in 0 s");
  });

  it("name the cause of an FC mode change in words, never the raw code", () => {
    const fcBanner = (cause: string) => ({ textKey: "pilot.banner.fc_mode_change", args: { mode: "RTL", cause }, untilMs: null });
    expect(visibleBanner(fcBanner("rc_or_failsafe"), 0, 0)).toBe("FC switched to RTL: RC switch or failsafe");
    expect(visibleBanner(fcBanner("fc"), 0, 0)).toBe("FC switched to RTL: flight controller");
    expect(visibleBanner(fcBanner("new_code"), 0, 0)).toBe("FC switched to RTL: —");
  });
});

describe("mode sheet hold texts", () => {
  it("say what a hold does", () => {
    expect(modeHoldLabel("avoid", "off", "Off")).toBe("Hold to turn avoidance off");
    expect(modeHoldLabel("control_mode", "ados", "ADOS")).toBe("Hold to switch to ADOS");
    expect(modeHoldLabel("profile", "x", "X")).toBe("X (hold)");
  });

  it("warn while avoidance is off", () => {
    expect(avoidOffWarning("off")).toBe("Obstacle avoidance off: the drone will not brake for obstacles.");
    expect(avoidOffWarning("brake")).toBeNull();
    expect(avoidOffWarning(null)).toBeNull();
  });
});
