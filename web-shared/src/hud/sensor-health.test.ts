import { describe, expect, it } from "vitest";

import spec from "../cockpit-spec/sensor-health.json";
import tokens from "../cockpit-spec/tokens.json";
import cases from "./fixtures/sensor-health-cases.json";
import { parsePilotState } from "./cockpit-frames";
import {
  cameraForLeg,
  cameraTabWord,
  cameraWord,
  flowWord,
  lidarWord,
  parseCameraHealth,
  parseFlowHealth,
  parseLidarHealth,
  parseRangeHealth,
  parseSensorHealth,
  parseSensorsHealth,
  rangeWord,
  sensorRowWords,
  type SensorWord,
} from "./sensor-health";

const STATE_HEX = tokens.state as Record<string, string>;
const plain = (w: SensorWord | null) => (w ? { state: w.state, text: w.text, color: w.color } : null);

function run(fn: string, input: unknown): unknown {
  const i = input as Record<string, unknown>;
  switch (fn) {
    case "flowWord":
      return plain(flowWord(parseFlowHealth(input)));
    case "rangeWord":
      return plain(rangeWord(parseRangeHealth(input)));
    case "lidarWord":
      return plain(lidarWord(parseLidarHealth(input)));
    case "cameraWord": {
      const c = parseCameraHealth(input);
      return c ? plain(cameraWord(c)) : null;
    }
    case "sensorRowWords":
      return sensorRowWords(parseSensorHealth(input)).map((w) => w.text);
    case "cameraTabWord":
      return plain(cameraTabWord(parseCameraHealth(i.camera), i.label as string));
    case "cameraForLeg": {
      const cams = (i.cameras as unknown[]).map(parseCameraHealth).filter((c) => c !== null);
      return cameraForLeg(cams, i.leg as { id: string; role: string | null })?.id ?? null;
    }
    default:
      throw new Error(`unknown case fn ${fn}`);
  }
}

describe("sensor-health cases", () => {
  for (const c of cases) {
    it(c.name, () => {
      expect(run(c.fn, c.input)).toEqual(c.expected);
    });
  }
});

describe("sensor-health spec", () => {
  it("colours every state with a tokens.json state colour", () => {
    for (const table of Object.values(spec.states)) {
      for (const entry of Object.values(table)) expect(STATE_HEX[entry.color], entry.color).toMatch(/^#/);
    }
  });

  it("returns the token hex with the colour name", () => {
    const w = lidarWord(parseLidarHealth({ state: "degraded", rate_hz: 4 }));
    expect(w.hex).toBe(STATE_HEX.avoiding);
  });
});

describe("sensor-health parsers", () => {
  it("reads the pilot kind's sensors block raw and parses it on demand", () => {
    const p = parsePilotState({ owner: "idle", sensors: { flow: { state: "low", quality: 40 }, range: null, lidar: { state: "ok", rate_hz: 10 } } });
    const s = parseSensorHealth(p?.sensors);
    expect(s?.flow).toEqual({ state: "low", quality: 40, rateHz: null, ageMs: null });
    expect(s?.range.state).toBe("unknown");
    expect(s?.lidar.rateHz).toBe(10);
    expect(parseSensorHealth(parsePilotState({ owner: "idle" })?.sensors)).toBeNull();
  });

  it("reads the sensors route's health with its cameras", () => {
    const h = parseSensorsHealth({
      flow: { state: "ok", quality: 200, rate_hz: 20, age_ms: 10, mount: null },
      range: { state: "ok", height_m: 1.2, age_ms: 10, mount: { pos_m: [0, 0, 0.05] } },
      lidar: { state: "absent", rate_hz: null, health: null, age_ms: null },
      cameras: [{ id: "front", leg: "main", role: "front", state: "live", fps: 30, age_ms: 30 }, { state: "live" }, "x"],
    });
    expect(h?.cameras.map((c) => c.id)).toEqual(["front"]);
    expect(h?.range.heightM).toBe(1.2);
    expect(parseSensorsHealth(null)).toBeNull();
    expect(parseSensorsHealth({ flow: {} })?.cameras).toEqual([]);
  });
});
