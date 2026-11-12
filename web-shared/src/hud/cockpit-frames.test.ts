import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

import {
  decodeBase64Into,
  decodeGridCode,
  gridCodeFor,
  parseArFrame,
  parseDepthFrame,
  parseMission,
  parseObstaclesFrame,
  parsePilotState,
  parseSurfacesFrame,
} from "./cockpit-frames";

// The golden fixtures the agent's own tests use, read in place.
const FIXTURES = path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures");
const fixture = (name: string): unknown => JSON.parse(readFileSync(path.join(FIXTURES, name), "utf8"));

describe("parseArFrame on the golden fixture", () => {
  const f = parseArFrame(fixture("ar-frame.json"));

  it("parses every item with its key", () => {
    expect(f).not.toBeNull();
    const cam = f!.cameras.main;
    expect(cam.frameW).toBe(1280);
    expect(cam.frameH).toBe(720);
    expect(cam.items.map((i) => i.k)).toEqual([
      "wp:4",
      "wp:5",
      "leg:active",
      "leg:next",
      "home",
      "fpv",
      "fpath",
      "fgoal",
      "orbit",
      "edge:home",
    ]);
    const next = cam.items[0];
    expect(next).toMatchObject({ t: "wp", seq: 4, role: "next", distM: 120, etaS: 42, gx: 0.52, gy: 0.63 });
    expect(cam.items[6]).toMatchObject({ t: "fpath", state: "detour" });
    expect(cam.items[9]).toMatchObject({ t: "edge", of: "home", ang: 2.6, distM: 35 });
  });

  it("carries the mission copy", () => {
    expect(f!.mission).toMatchObject({ state: "active", seq: 4, total: 6, itemReached: 3, source: "mission_state", landingSeq: false });
  });

  it("derives a key when the producer omits it and drops malformed items", () => {
    const g = parseArFrame({
      ts_ms: 1,
      cameras: { main: { frame_w: 10, frame_h: 10, items: [{ t: "wp", seq: 2, role: "after", x: 0.1, y: 0.2 }, { t: "leg", role: "active", pts: [[0, 0]] }, { t: "nope" }] } },
    });
    expect(g!.cameras.main.items).toHaveLength(1);
    expect(g!.cameras.main.items[0].k).toBe("wp:2");
  });

  it("rejects a frame without cameras", () => {
    expect(parseArFrame({ v: 1 })).toBeNull();
    expect(parseArFrame(null)).toBeNull();
  });
});

describe("parsePilotState on the golden fixture", () => {
  const p = parsePilotState(fixture("pilot-frame.json"));

  it("parses the pilot fields and the flow additions", () => {
    expect(p).not.toBeNull();
    expect(p!.owner).toBe("skill");
    expect(p!.skill).toEqual({ pluginId: "com.example.follow", skillId: "follow-me", label: "Follow" });
    expect(p!.vehicleClass).toBe("multirotor");
    expect(p!.controlMode).toBe("ados");
    expect(p!.fcMode).toBe(4);
    expect(p!.avoidState).toBe("detour");
    expect(p!.goalNed).toEqual([12, -3.5, -4]);
    expect(p!.planNed).toHaveLength(4);
    expect(p!.setpointNed).toEqual([10.8, -2.6, -4]);
    expect(p!.readiness.map((r) => [r.id, r.ok, r.severity])).toEqual([
      ["fc_profile", true, "block"],
      ["position", true, "block"],
      ["agl", false, "warn"],
    ]);
    expect(p!.mission).toMatchObject({ state: "none", total: 0, source: "fallback" });
    expect(p!.inputSession).toMatchObject({ src: "mc", label: "Mission Control" });
    expect(p!.banner).toEqual({ textKey: "pilot.banner.ados_engaged", args: { source: "GCS" }, untilMs: 1793960003000 });
    expect(p!.gate).toBeNull();
    expect(p!.sequence).toBeNull();
    expect(p!.surfaceOps).toEqual(["goto"]);
    expect([p!.aglM, p!.aglSource]).toEqual([4.2, "rangefinder"]);
    expect(p!.statustext).toEqual([{ severity: 4, text: "PreArm: Compass not calibrated", ageMs: 1200 }]);
    // The agent omits `fc_failsafe` while false and sends `failsafe_cause: null`.
    expect([p!.fcFailsafe, p!.failsafeCause]).toEqual([false, null]);
  });

  it("reads an FC failsafe and its cause", () => {
    const raw = { ...(fixture("pilot-frame.json") as object), owner: "fc_mode", fc_failsafe: true, failsafe_cause: "rc_or_failsafe" };
    expect(parsePilotState(raw)).toMatchObject({ fcFailsafe: true, failsafeCause: "rc_or_failsafe" });
    expect(parsePilotState({ owner: "fc_mode", fc_failsafe: true, failsafe_cause: "fc" })).toMatchObject({ fcFailsafe: true, failsafeCause: "fc" });
  });

  it("reads a mission the pilot holds paused", () => {
    const p2 = parsePilotState({ owner: "pilot", mission: { state: "paused", seq: 3, total: 6 } });
    expect(p2!.owner).toBe("pilot");
    expect(p2!.mission).toMatchObject({ state: "paused", seq: 3, total: 6 });
  });

  it("rejects an unknown owner", () => {
    expect(parsePilotState({ owner: "boss" })).toBeNull();
  });

  it("keeps an unknown arm and landed state unknown", () => {
    expect(parsePilotState({ owner: "idle", armed: null, landed: null })).toMatchObject({ armed: null, landed: null });
    expect(parsePilotState({ owner: "idle" })).toMatchObject({ armed: null, landed: null });
    expect(parsePilotState({ owner: "idle", armed: false, landed: true })).toMatchObject({ armed: false, landed: true });
  });

  it("caps the plan at 16 points", () => {
    const plan = Array.from({ length: 30 }, (_, i) => [i, i]);
    expect(parsePilotState({ owner: "pilot", plan_ned: plan })!.planNed).toHaveLength(16);
  });
});

describe("parseMission", () => {
  it("accepts the short pilot form", () => {
    expect(parseMission({ current_seq: 3, total: 5 })).toMatchObject({ seq: 3, total: 5, state: null });
  });
});

describe("parseObstaclesFrame on the golden fixture", () => {
  const o = parseObstaclesFrame(fixture("obstacles-frame.json"));

  it("parses 72 sectors and the main grid", () => {
    expect(o).not.toBeNull();
    expect(o!.sectorsM).toHaveLength(72);
    expect(o!.sectorsM[0]).toBe(3.1);
    expect(o!.sectorsM[3]).toBeNull();
    const g = o!.cameras.main;
    expect(g.cols).toBe(32);
    expect(g.rows).toBe(18);
    const near = new Uint8Array(576);
    expect(decodeBase64Into(g.near, near)).toBe(576);
    const meas = new Uint8Array(72);
    expect(decodeBase64Into(g.measured!, meas)).toBe(72);
  });
});

describe("depth and surfaces on the golden fixtures", () => {
  it("parses the depth fixture", () => {
    const d = parseDepthFrame(fixture("depth-frame.json"));
    expect(d).not.toBeNull();
    const g = Object.values(d!.cameras)[0];
    expect(g.cols * g.rows).toBe(576);
    const near = new Uint8Array(576);
    expect(decodeBase64Into(g.near, near)).toBe(576);
  });

  it("parses the surfaces fixture", () => {
    const s = parseSurfacesFrame(fixture("surfaces-frame.json"));
    expect(s).not.toBeNull();
    expect(s!.zones.length).toBeGreaterThan(0);
  });
});

describe("depth and surfaces", () => {
  it("parses a depth frame and its scale source", () => {
    const d = parseDepthFrame({ ts_ms: 5, scale_source: "lidar", cameras: { main: { cols: 2, rows: 1, near: "AQI=", measured: "Aw==" } } });
    expect(d).toMatchObject({ tsMs: 5, scaleSource: "lidar" });
    expect(d!.cameras.main.cols).toBe(2);
    expect(parseDepthFrame({ cameras: {} })!.scaleSource).toBe("none");
  });

  it("parses landing zones and drops degenerate ones", () => {
    const s = parseSurfacesFrame({
      v: 1,
      ts_ms: 9,
      camera_id: "down",
      zones: [
        { pts: [[0.1, 0.1], [0.2, 0.1], [0.2, 0.2]], score: 0.8, slope_deg: 3 },
        { pts: [[0, 0]], score: 1 },
      ],
    });
    expect(s!.cameraId).toBe("down");
    expect(s!.zones).toEqual([{ pts: [[0.1, 0.1], [0.2, 0.1], [0.2, 0.2]], score: 0.8, slopeDeg: 3 }]);
  });
});

describe("grid codes", () => {
  it("decodes the log scale end points", () => {
    expect(decodeGridCode(0)).toBeNull();
    expect(decodeGridCode(1)).toBeCloseTo(0.2, 6);
    expect(decodeGridCode(255)).toBeCloseTo(60, 6);
  });

  it("gridCodeFor is the smallest code at or beyond a distance", () => {
    for (const m of [0.5, 2, 5, 12]) {
      const c = gridCodeFor(m);
      expect(decodeGridCode(c)!).toBeGreaterThanOrEqual(m - 1e-9);
      expect(decodeGridCode(c - 1)!).toBeLessThan(m);
    }
  });

  it("decodes standard and url-safe base64 without overrun", () => {
    const out = new Uint8Array(3);
    expect(decodeBase64Into("AP8B", out)).toBe(3);
    expect(Array.from(out)).toEqual([0, 255, 1]);
    expect(decodeBase64Into("AP-_", out)).toBe(3);
    expect(Array.from(out)).toEqual([0, 255, 191]);
    const small = new Uint8Array(1);
    expect(decodeBase64Into("AP8B", small)).toBe(1);
  });
});
