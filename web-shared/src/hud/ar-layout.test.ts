import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

import { arStatusChip, layoutAr, type ArLayoutOptions } from "./ar-layout";
import { gridCodeFor, parseArFrame, parseMission, type ArCamera } from "./cockpit-frames";
import { DrawOpBuffer, containRect, lerpOps, layoutCells, layoutSectorArc } from "./layer-layout";

const FIXTURE = path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures/ar-frame.json");
const frame = parseArFrame(JSON.parse(readFileSync(FIXTURE, "utf8")))!;
const cam: ArCamera = frame.cameras.main;
const rect = { x: 0, y: 60, w: 1280, h: 720 };

function opts(over: Partial<ArLayoutOptions> = {}): ArLayoutOptions {
  return { level: "full", groups: { route: true, motion: true }, mission: frame.mission, avoid: null, showLoaded: true, ...over };
}

function keys(o: Partial<ArLayoutOptions> = {}, c: ArCamera = cam): string[] {
  const buf = new DrawOpBuffer();
  layoutAr(c, rect, opts(o), buf);
  return buf.ops.slice(0, buf.n).map((op) => `${op.kind}:${op.k}`);
}

describe("layoutAr on the golden frame", () => {
  it("places the next waypoint in rendered-rect pixels with its chip", () => {
    const buf = new DrawOpBuffer();
    layoutAr(cam, rect, opts(), buf);
    const tri = buf.ops.slice(0, buf.n).find((o) => o.k === "wp:4" && o.kind === "tri")!;
    expect(tri.x).toBeCloseTo(0.52 * 1280);
    expect(tri.y).toBeCloseTo(60 + 0.41 * 720);
    expect(tri.r).toBe(24 + 40 * 0.25);
    expect(tri.fill).not.toBeNull();
    const chip = buf.ops.slice(0, buf.n).find((o) => o.k === "wp:4#chip")!;
    expect(chip.text).toBe("WP4 · 120 m · 0:42");
    const after = buf.ops.slice(0, buf.n).find((o) => o.k === "wp:5")!;
    expect(after.dash).toBeGreaterThan(0);
    expect(after.fill).toBeNull();
  });

  it("draws every group at Full and nothing at Off", () => {
    const k = keys();
    for (const want of ["line:leg:active", "line:leg:next", "pad:home", "fpv:fpv", "line:fpath", "circle:fgoal", "line:orbit", "arrow:edge:home"]) {
      expect(k).toContain(want);
    }
    expect(k.filter((x) => x.startsWith("chevron:orbit"))).toHaveLength(3);
    expect(keys({ level: "off" })).toEqual([]);
  });

  it("honours the route and motion layer flags", () => {
    const routeOnly = keys({ groups: { route: true, motion: false } });
    expect(routeOnly.some((k) => k.includes("fpv") || k.includes("fpath") || k.includes("orbit"))).toBe(false);
    expect(routeOnly).toContain("tri:wp:4");
    const motionOnly = keys({ groups: { route: false, motion: true } });
    expect(motionOnly.some((k) => k.includes("wp:") || k.includes("leg:") || k.includes("home"))).toBe(false);
  });

  it("Route level drops other waypoints and future legs", () => {
    const c: ArCamera = {
      ...cam,
      items: [
        ...cam.items,
        { t: "wp", k: "wp:9", seq: 9, role: "other", x: 0.7, y: 0.3, gx: null, gy: null, distM: null, etaS: null, size: 0 },
        { t: "leg", k: "leg:future", role: "future", pts: [[0.6, 0.6], [0.7, 0.5], [0.8, 0.4]] },
      ],
    };
    expect(keys({}, c)).toContain("circle:wp:9");
    expect(keys({}, c)).toContain("line:leg:future#1");
    expect(keys({ level: "route" }, c).some((k) => k.includes("wp:9") || k.includes("leg:future"))).toBe(false);
    expect(keys({ level: "instruments" }, c)).toEqual(keys({ level: "full" }, c));
  });

  it("follows the mission state rules", () => {
    const m = (state: string) => parseMission({ state, seq: 4, total: 6 });
    const paused = new DrawOpBuffer();
    layoutAr(cam, rect, opts({ mission: m("paused") }), paused);
    const leg = paused.ops.slice(0, paused.n).find((o) => o.k === "leg:active")!;
    expect(leg.color).toBe("#909090");
    expect(leg.dash).toBeGreaterThan(0);
    expect(paused.ops.slice(0, paused.n).find((o) => o.k === "wp:4")!.fill).toBeNull();

    for (const hidden of ["returning", "complete", "none"]) {
      const k = keys({ mission: m(hidden) });
      expect(k.some((x) => x.includes("wp:") || x.includes("leg:"))).toBe(false);
    }
    const ret = new DrawOpBuffer();
    layoutAr(cam, rect, opts({ mission: m("returning") }), ret);
    expect(ret.ops.slice(0, ret.n).find((o) => o.k === "home")!.r).toBeGreaterThan(12);

    expect(keys({ mission: m("loaded"), showLoaded: false }).some((x) => x.includes("wp:"))).toBe(false);
    const loaded = new DrawOpBuffer();
    layoutAr(cam, rect, opts({ mission: m("loaded") }), loaded);
    expect(loaded.ops.slice(0, loaded.n).find((o) => o.k === "leg:active")!.color).toBe("#FFFFFF");
  });

  it("colours the follow path by the pilot's avoid state", () => {
    const at = (avoid: ArLayoutOptions["avoid"]) => {
      const b = new DrawOpBuffer();
      layoutAr(cam, rect, opts({ avoid }), b);
      return b.ops.slice(0, b.n);
    };
    expect(at(null).find((o) => o.k === "fpath")!.color).toBe("#F59E0B"); // item says detour
    expect(at("clear").find((o) => o.k === "fpath")!.color).toBe("#22D3EE");
    const blocked = at("blocked");
    expect(blocked.find((o) => o.k === "fpath")!.color).toBe("#EF4444");
    expect(blocked.some((o) => o.kind === "cross")).toBe(true);
  });

  it("keeps a blocked goal drawn, in the blocked colour", () => {
    const at = (avoid: ArLayoutOptions["avoid"], c: ArCamera = cam) => {
      const b = new DrawOpBuffer();
      layoutAr(c, rect, opts({ avoid }), b);
      return b.ops.slice(0, b.n).find((o) => o.k === "fgoal")!;
    };
    expect(at("clear").color).toBe("#22D3EE");
    expect(at("blocked").color).toBe("#EF4444");
    // Without a pilot state the goal takes the path's state from the agent.
    const blockedPath: ArCamera = {
      ...cam,
      items: cam.items.map((i) => (i.t === "fpath" ? { ...i, state: "blocked" as const } : i)),
    };
    expect(at(null).color).toBe("#22D3EE"); // the golden path is a detour
    expect(at(null, blockedPath).color).toBe("#EF4444");
  });

  it("keeps edge arrows inside the rect inset", () => {
    for (const ang of [0, 1, 2.6, Math.PI, -1.2]) {
      const c: ArCamera = { ...cam, items: [{ t: "edge", k: "edge:home", of: "home", seq: null, ang, distM: 35 }] };
      const b = new DrawOpBuffer();
      layoutAr(c, rect, opts(), b);
      const a = b.ops[0];
      expect(a.x).toBeGreaterThanOrEqual(rect.x + 16);
      expect(a.x).toBeLessThanOrEqual(rect.x + rect.w - 16);
      expect(a.y).toBeGreaterThanOrEqual(rect.y + 16);
      expect(a.y).toBeLessThanOrEqual(rect.y + rect.h - 16);
    }
  });

  it("reports the frame chip", () => {
    expect(arStatusChip(frame, cam, false)).toBeNull();
    expect(arStatusChip({ ...frame, approx: true }, cam, false)).toBe("AR ≈");
    expect(arStatusChip(frame, cam, true)).toBe("AR unavailable");
    expect(arStatusChip({ ...frame, reason: "no_pose" }, cam, false)).toBe("AR unavailable");
    expect(arStatusChip(null, null, false)).toBe("AR unavailable");
  });
});

describe("layer-layout", () => {
  it("letterboxes into the contain rect", () => {
    expect(containRect(1920, 1080, 1280, 720)).toEqual({ x: 0, y: 0, w: 1920, h: 1080 });
    expect(containRect(1000, 1000, 1280, 720)).toEqual({ x: 0, y: 218.75, w: 1000, h: 562.5 });
  });

  it("interpolates keyed ops and fades unmatched ones", () => {
    const a = new DrawOpBuffer();
    const b = new DrawOpBuffer();
    const out = new DrawOpBuffer();
    const pa = a.push("fpv", "fpv");
    pa.x = 0;
    pa.y = 0;
    const gone = a.push("old", "circle");
    gone.x = 5;
    const pb = b.push("fpv", "fpv");
    pb.x = 100;
    pb.y = 50;
    b.push("new", "circle");
    lerpOps(a, b, 0.5, 0.25, out);
    const ops = out.ops.slice(0, out.n);
    expect(ops.find((o) => o.k === "fpv")).toMatchObject({ x: 50, y: 25, alpha: 1 });
    expect(ops.find((o) => o.k === "new")!.alpha).toBeCloseTo(0.25);
    expect(ops.find((o) => o.k === "old")!.alpha).toBeCloseTo(0.75);
    lerpOps(a, b, 1, 1, out);
    expect(out.ops.slice(0, out.n).some((o) => o.k === "old")).toBe(false);
  });

  it("recycles op objects across frames", () => {
    const b = new DrawOpBuffer();
    layoutAr(cam, rect, opts(), b);
    const first = b.ops[0];
    const n = b.ops.length;
    b.clear();
    layoutAr(cam, rect, opts(), b);
    expect(b.ops[0]).toBe(first);
    expect(b.ops.length).toBe(n);
  });

  it("draws cells within range only, red near, solid only when measured", () => {
    // 4×1 grid: unknown, 1 m measured, 3 m unmeasured, 10 m.
    const near = new Uint8Array([0, gridCodeFor(1), gridCodeFor(3), gridCodeFor(10)]);
    const measured = new Uint8Array([0b0010]);
    const b = new DrawOpBuffer();
    layoutCells(near, measured, 4, 1, { x: 0, y: 0, w: 400, h: 100 }, { maxM: 5, nearM: 2, mode: "solid" }, b);
    const ops = b.ops.slice(0, b.n);
    expect(ops).toHaveLength(2);
    expect(ops[0]).toMatchObject({ x: 100, w: 100, color: "#EF4444", fill: "#EF4444" });
    expect(ops[1]).toMatchObject({ x: 200, color: "#F59E0B", fill: null });
    const outline = new DrawOpBuffer();
    layoutCells(near, measured, 4, 1, { x: 0, y: 0, w: 400, h: 100 }, { maxM: 5, nearM: 2, mode: "outline" }, outline);
    expect(outline.ops[0].fill).toBeNull();
  });

  it("lays out the front obstacle arc and the nearest label", () => {
    const sectors: (number | null)[] = Array.from({ length: 72 }, () => null);
    sectors[0] = 1.5;
    sectors[70] = 4;
    sectors[36] = 0.5; // behind: not on the front strip
    const b = new DrawOpBuffer();
    layoutSectorArc(sectors, { x: 0, y: 0, w: 1000, h: 500 }, { maxM: 5, nearM: 2 }, b);
    const ops = b.ops.slice(0, b.n);
    expect(ops.filter((o) => o.kind === "line")).toHaveLength(2);
    expect(ops.find((o) => o.kind === "chip")!.text).toBe("1.5 m");
  });
});
