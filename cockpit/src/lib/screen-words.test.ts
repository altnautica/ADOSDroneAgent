import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

import i18n from "@/shared/i18n";
import {
  hubTransportWord,
  jobStateKey,
  loraRuleKey,
  loraSourceKey,
  peripheralStateKey,
  sensorKindKey,
  sessionStateKey,
} from "@/lib/screen-words";

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
    expect(sensorKindKey("radar")).toBeNull();
    expect(jobStateKey("warp")).toBeNull();
    expect(peripheralStateKey({ connected: false, state: "sleeping" })).toBeNull();
    expect(peripheralStateKey({ connected: true, state: "sleeping" })).toBe("cockpit.peripherals.state.connected");
  });

  it("reads the hub link as a word plus its target", () => {
    expect(hubTransportWord("tcp:sim-host:7600")).toEqual({ key: "cockpit.sensors.hubSimulated", target: "sim-host:7600" });
    expect(hubTransportWord("spi")).toBeNull();
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

  it("lets the operator cancel a waiting radio bind", () => {
    expect(src("components/screens/pair-screen.tsx")).toContain('"/api/wfb/pair/local-bind/cancel"');
  });
});
