import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

import i18n from "@/shared/i18n";
import {
  captureStateKey,
  hubTransportWord,
  jobStateKey,
  loraRuleKey,
  loraSourceKey,
  peripheralStateKey,
  sensorKindKey,
  serviceStateKey,
  sessionStateKey,
} from "@/lib/screen-words";
import { profileWordKey, roleWordKey } from "@/lib/profile-words";

const en = (key: string) => i18n.t(key as never) as unknown as string;
const src = (rel: string) => readFileSync(path.resolve(__dirname, "..", rel), "utf8");

describe("screen words", () => {
  it("keys every word of the agent's closed sets, and only those", () => {
    for (const key of [
      sensorKindKey("mag"),
      jobStateKey("interrupted"),
      sessionStateKey("stalled"),
      loraRuleKey("max_power"),
      loraSourceKey("network.regulatory.region"),
      peripheralStateKey({ connected: false, state: "NOT_DETECTED" }),
    ]) {
      expect(key).not.toBeNull();
      expect(i18n.exists(key!)).toBe(true);
    }
    expect(sensorKindKey("radar")).toBe("panels.state.unrecognised");
    expect(jobStateKey("warp")).toBe("panels.state.unrecognised");
    expect(peripheralStateKey({ connected: false, state: "sleeping" })).toBe("panels.state.unrecognised");
    expect(sensorKindKey(null)).toBeNull();
    expect(peripheralStateKey({ connected: true, state: "sleeping" })).toBe("cockpit.peripherals.state.connected");
  });

  it("names service and capture states, and reads any other word as unrecognised", () => {
    expect(en(serviceStateKey("ACTIVE")!)).toBe("Running");
    expect(en(serviceStateKey("activating")!)).toBe("Starting");
    expect(serviceStateKey("warp")).toBe("panels.state.unrecognised");
    expect(serviceStateKey(null)).toBeNull();
    expect(captureStateKey("recording")).toBe("capture.state.recording");
    expect(captureStateKey("warp")).toBe("panels.state.unrecognised");
  });

  it("reads an unknown profile or role as Unrecognised, never a lower-case key", () => {
    expect(en(roleWordKey("satellite"))).toBe("Unrecognised");
    expect(en(profileWordKey("toaster"))).toBe("Unrecognised");
  });

  it("reads the hub link as a word plus its target", () => {
    expect(hubTransportWord("tcp:sim-host:7600")).toEqual({ key: "cockpit.sensors.hubSimulated", target: "sim-host:7600" });
    expect(hubTransportWord("spi")).toEqual({ key: "panels.state.unrecognised", target: "" });
    expect(hubTransportWord(null)).toBeNull();
  });
});

describe("wording", () => {
  it("says stick calibration on the Controls row", () => {
    expect(en("cockpit.settingsSections.root.controlsSummary")).toContain("stick calibration");
  });

  it("explains a partial parameter save without the message name", () => {
    expect(src("components/screens/params-screen.tsx")).toContain('"cockpit.params.savedPartialHint"');
    expect(en("cockpit.params.savedPartialHint")).not.toMatch(/[A-Z]+_[A-Z]+/);
  });
});
