import { readFileSync } from "node:fs";
import path from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { parseArFrame, parseDepthFrame, parseObstaclesFrame, type DepthFrame } from "./cockpit-frames";
import { chipTexts, LayerEngine, LayerFrameStore, type LayerConfig } from "./layer-engine";

const FIXTURES = path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures");
const ar = parseArFrame(JSON.parse(readFileSync(path.join(FIXTURES, "ar-frame.json"), "utf8")))!;
const obstacles = parseObstaclesFrame(JSON.parse(readFileSync(path.join(FIXTURES, "obstacles-frame.json"), "utf8")))!;
/** Depth frames recorded from a live rendered run, one per scale source. */
const recordedDepth = (source: "lidar" | "none") =>
  parseDepthFrame(JSON.parse(readFileSync(path.join(FIXTURES, `depth-frame-recorded-${source}.json`), "utf8")))!;

/** A 2D context that counts paints (clearRect per paint) and records text. */
function fakeCanvas() {
  const calls = { clears: 0, texts: [] as string[], strokes: 0 };
  const noop = () => undefined;
  const ctx = {
    canvas: { width: 0, height: 0 },
    setTransform: noop,
    clearRect: () => {
      calls.clears++;
    },
    beginPath: noop,
    moveTo: noop,
    lineTo: noop,
    closePath: noop,
    rect: noop,
    arc: noop,
    roundRect: noop,
    fill: noop,
    stroke: () => {
      calls.strokes++;
    },
    strokeText: noop,
    fillText: (t: string) => {
      calls.texts.push(t);
    },
    measureText: (t: string) => ({ width: t.length * 7 }),
    setLineDash: noop,
  };
  const canvas = { width: 0, height: 0, getContext: () => ctx };
  ctx.canvas = canvas;
  return { canvas: canvas as unknown as HTMLCanvasElement, calls };
}

let rafQueue: ((t: number) => void)[] = [];
let clock = 0;
function frame(dtMs = 16.7) {
  clock += dtMs;
  const q = rafQueue;
  rafQueue = [];
  for (const cb of q) cb(clock);
}

const CFG: LayerConfig = {
  cameraId: "main",
  layers: new Set(["ar_route", "ar_motion", "obstacles"]),
  level: "full",
  videoW: 1280,
  videoH: 720,
  mission: null,
  avoid: null,
  showLoaded: true,
  depthRangeM: 5,
  depthMode: "solid",
  fpsCap: 0,
};

beforeEach(() => {
  rafQueue = [];
  clock = 1000;
  vi.stubGlobal("requestAnimationFrame", (cb: (t: number) => void) => {
    rafQueue.push(cb);
    return rafQueue.length;
  });
  vi.stubGlobal("cancelAnimationFrame", () => {
    rafQueue = [];
  });
  vi.spyOn(performance, "now").mockImplementation(() => clock);
});
afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

function setup(cfg: Partial<LayerConfig> = {}) {
  const store = new LayerFrameStore();
  const engine = new LayerEngine(store, { ...CFG, ...cfg });
  const { canvas, calls } = fakeCanvas();
  engine.attach(canvas);
  engine.resize(1280, 720, 1);
  engine.start();
  return { store, engine, calls };
}

describe("LayerEngine", () => {
  it("paints only when something changed", () => {
    const { store, calls } = setup();
    frame();
    const afterFirst = calls.clears;
    expect(afterFirst).toBe(1);
    for (let i = 0; i < 10; i++) frame();
    expect(calls.clears).toBe(afterFirst);
    store.putAr(ar, clock);
    store.putObstacles(obstacles, clock);
    frame();
    expect(calls.clears).toBe(afterFirst + 1);
  });

  it("keeps painting while an interpolation runs, then idles", () => {
    const { store, calls } = setup();
    store.putAr(ar, clock);
    frame();
    clock += 50;
    store.putAr({ ...ar, tsMs: ar.tsMs + 50 }, clock);
    const before = calls.clears;
    for (let i = 0; i < 3; i++) frame(16);
    expect(calls.clears - before).toBe(3);
    for (let i = 0; i < 20; i++) frame(16);
    const settled = calls.clears;
    for (let i = 0; i < 5; i++) frame(16);
    expect(calls.clears).toBe(settled);
  });

  it("caps paints at the configured rate", () => {
    const { store, calls } = setup({ fpsCap: 30 });
    frame();
    const start = calls.clears;
    for (let i = 0; i < 60; i++) {
      store.putAr(ar, clock); // a change every display frame
      frame(1000 / 120);
    }
    // 60 frames at 120 Hz is 0.5 s: at most ~15 paints at 30 fps.
    expect(calls.clears - start).toBeLessThanOrEqual(16);
    expect(calls.clears - start).toBeGreaterThanOrEqual(13);
  });

  it("shows unavailable chips for stale or missing layers", () => {
    const { store, calls } = setup();
    store.putAr(ar, clock);
    store.putObstacles(obstacles, clock);
    frame();
    expect(calls.texts).toContain("WP4 · 120 m · 0:42");
    expect(calls.texts).not.toContain("AR unavailable");
    calls.texts.length = 0;
    frame(2100); // past HIDE_AFTER_MS: a staleness step repaints
    expect(calls.texts).toContain("AR unavailable");
    expect(calls.texts).toContain("Obstacles unavailable");
    expect(calls.texts).not.toContain("WP4 · 120 m · 0:42");
  });

  it("hands the chips to the host instead of painting them under its banners", () => {
    const masks: number[] = [];
    const { store, engine, calls } = setup({ onChips: (m) => masks.push(m) });
    store.putAr(ar, clock);
    store.putObstacles(obstacles, clock);
    frame();
    expect(masks).toEqual([]);
    frame(2100);
    expect(masks).toHaveLength(1);
    expect(chipTexts(masks[0])).toEqual(["Obstacles unavailable", "AR unavailable"]);
    expect(calls.texts).not.toContain("Obstacles unavailable");
    expect(calls.texts).not.toContain("AR unavailable");
    frame(100);
    expect(masks).toHaveLength(1);
    engine.stop();
    expect(masks).toEqual([masks[0], 0]);
  });

  it("draws nothing for a camera the frame does not carry", () => {
    const { store, calls } = setup({ cameraId: "down", layers: new Set(["ar_route"]) });
    store.putAr(ar, clock);
    frame();
    expect(calls.strokes).toBe(0);
    expect(calls.texts).toEqual(["AR unavailable"]);
  });

  it("stops the loop", () => {
    const { engine } = setup();
    frame();
    engine.stop();
    expect(rafQueue).toHaveLength(0);
  });

  it("treats a repeated ts_ms as a keep-alive: fresh, no re-interpolation", () => {
    const { store, calls } = setup({ layers: new Set(["ar_route"]) });
    store.putAr(ar, clock);
    frame();
    // Keep-alives once a second for 5 s: the route never hides.
    for (let i = 0; i < 5; i++) {
      for (let j = 0; j < 60; j++) frame(16.7);
      store.putAr(ar, clock);
      const v = store.ar.version;
      frame();
      expect(store.ar.version).toBe(v);
    }
    calls.texts.length = 0;
    store.putAr(ar, clock);
    frame();
    expect(calls.texts).toContain("WP4 · 120 m · 0:42");
    expect(calls.texts).not.toContain("AR unavailable");
    // The previous frame is still the original, not the keep-alive.
    expect(store.arPrev.data).toBeNull();
  });

  it("dims and hides each cell layer on its own clock", () => {
    const depth: DepthFrame = { tsMs: 1, scaleSource: "lidar", cameras: obstacles.cameras };
    const { store, calls } = setup({ layers: new Set(["obstacles", "depth"]) });
    store.putObstacles(obstacles, clock);
    frame();
    const obstacleTexts = calls.texts.filter((t) => !t.endsWith("unavailable"));
    expect(obstacleTexts.length).toBeGreaterThan(0);
    expect(calls.texts).toContain("Depth unavailable");
    // Depth keeps arriving; obstacles go silent past the hide threshold.
    for (let i = 0; i < 25; i++) {
      store.putDepth({ ...depth, tsMs: 100 + i }, clock);
      frame(100);
    }
    calls.texts.length = 0;
    store.putDepth({ ...depth, tsMs: 999 }, clock);
    frame();
    expect(calls.texts).toContain("Obstacles unavailable");
    expect(calls.texts).not.toContain("Depth unavailable");
    for (const t of obstacleTexts) expect(calls.texts).not.toContain(t);
  });

  it.each(["lidar", "none"] as const)("draws a recorded %s-scaled depth frame on the served camera", (source) => {
    const depth = recordedDepth(source);
    expect(depth.scaleSource).toBe(source);
    expect(depth.cameras.main?.scaleSource).toBe(source);
    const { store, calls } = setup({ layers: new Set(["depth"]) });
    store.putDepth(depth, clock);
    frame();
    expect(calls.strokes).toBeGreaterThan(0);
    expect(calls.texts).not.toContain("Depth unavailable");
    if (source === "none") expect(calls.texts).toContain("Depth unscaled");
    else expect(calls.texts).not.toContain("Depth unscaled");
  });

  it("draws no depth cells with the range Off", () => {
    const { store, calls } = setup({ layers: new Set(["depth"]), depthRangeM: 0 });
    store.putDepth(recordedDepth("lidar"), clock);
    frame();
    expect(calls.strokes).toBe(0);
  });

  it("records paint timings", () => {
    const { store, engine } = setup();
    store.putAr(ar, clock);
    frame();
    expect(engine.stats().frames).toBe(1);
  });
});
