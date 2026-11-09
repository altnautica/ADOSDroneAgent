import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

import { gridMeasures, widthClassFor } from "@/lib/cockpit-layout";
import { COCKPIT_VIEWS, effectiveLayers, LAYER_LEVELS } from "@/lib/cockpit-view";
import { FLIGHT_PX, flightLayout, intersects, type FlightLayout, type Rect } from "@/lib/flight-layout";

const VIEWPORTS = [
  { name: "kiosk 1920x1080", width: 1920, height: 1080, kiosk: true },
  { name: "1920x1080", width: 1920, height: 1080, kiosk: false },
  { name: "1280x800", width: 1280, height: 800, kiosk: false },
  { name: "900x600", width: 900, height: 600, kiosk: false },
  { name: "760x480", width: 760, height: 480, kiosk: false },
  { name: "580x360", width: 580, height: 360, kiosk: false },
  { name: "kiosk 1280x720", width: 1280, height: 720, kiosk: true },
] as const;

function layoutsFor(vp: (typeof VIEWPORTS)[number]): { label: string; L: FlightLayout }[] {
  const widthClass = widthClassFor(vp.width, vp.kiosk);
  const out: { label: string; L: FlightLayout }[] = [];
  for (const glove of [false, true])
    for (const view of COCKPIT_VIEWS)
      for (const strip of [false, true])
        for (const gimbal of [false, true]) {
          const m = gridMeasures(widthClass, glove);
          out.push({
            label: `${vp.name} ${view}${strip ? " strip" : ""}${gimbal ? " gimbal" : ""}${glove ? " glove" : ""}`,
            L: flightLayout({ width: vp.width, height: vp.height, m, widthClass, view, flies: true, strip, gimbal, splitRatio: 0.6 }),
          });
        }
  return out;
}

const inside = (r: Rect, w: number, h: number) => r.x >= 0 && r.y >= 0 && r.x + r.w <= w + 1e-6 && r.y + r.h <= h + 1e-6;

describe("the flight screen zones (cockpit spec layout.json)", () => {
  for (const vp of VIEWPORTS) {
    it(`never overlap and stay on screen at ${vp.name}`, () => {
      for (const { label, L } of layoutsFor(vp)) {
        const zones: [string, Rect | null][] = [
          ["top bar", L.topBar],
          ["follow strip", L.strip],
          ["left column", L.leftColumn],
          ["right column", L.rightColumn],
          ["bottom-left card", L.card],
          ["telemetry row", L.bottomRow],
          ["skill picker", L.skillPicker],
          ["vision panel", L.visionPanel],
          ["divider", L.divider],
        ];
        const present = zones.filter((z): z is [string, Rect] => z[1] !== null);
        for (const [name, r] of present) {
          expect(inside(r, vp.width, vp.height), `${label}: ${name} off screen ${JSON.stringify(r)}`).toBe(true);
          expect(r.w > 0 && r.h > 0, `${label}: ${name} empty`).toBe(true);
        }
        for (let a = 0; a < present.length; a++)
          for (let b = a + 1; b < present.length; b++)
            expect(intersects(present[a][1], present[b][1]), `${label}: ${present[a][0]} overlaps ${present[b][0]}`).toBe(false);
        // The instruments draw clear of every zone.
        for (const [name, r] of present) expect(intersects(L.hud, r), `${label}: instruments under ${name}`).toBe(false);
        // The left column never sits under the top bar, and every column item fits.
        if (L.leftColumn) expect(L.leftColumn.y).toBeGreaterThanOrEqual(L.topBar.y + L.topBar.h);
        expect(L.rightColumn.y).toBeGreaterThanOrEqual(L.topBar.y + L.topBar.h);
        expect(L.rightItems).toEqual(expect.arrayContaining(["shutter", "mode", "settings"]));
        expect(L.bottomRow, `${label}: telemetry row`).not.toBeNull();
      }
    });

    it(`keeps STOP whole inside the viewport and the top bar at ${vp.name}`, () => {
      for (const { label, L } of layoutsFor(vp)) {
        expect(inside(L.stop, vp.width, vp.height), `${label}: STOP off screen`).toBe(true);
        expect(L.stop.x).toBeGreaterThanOrEqual(L.topBar.x);
        expect(L.stop.x + L.stop.w).toBeLessThanOrEqual(L.topBar.x + L.topBar.w);
        // The items that never move into the Menu fit the bar at its narrowest.
        const s = gridMeasures(widthClassFor(vp.width, vp.kiosk), false).scale;
        expect(L.topBar.w / s, `${label}: bar too narrow`).toBeGreaterThanOrEqual(FLIGHT_PX.barMinW);
      }
    });
  }

  it("fits the never-moving top-bar items on a 412 px portrait phone", () => {
    expect(412 - 2 * FLIGHT_PX.edge - 2 * 4).toBeGreaterThanOrEqual(FLIGHT_PX.barMinW);
  });

  it("drops the gimbal pad before zoom where the band is short", () => {
    const m = gridMeasures("medium", false);
    const L = flightLayout({ width: 760, height: 480, m, widthClass: "medium", view: "pilot", flies: true, strip: false, gimbal: true, splitRatio: 0.6 });
    expect(L.rightItems).not.toContain("gimbal");
    const big = flightLayout({ width: 1280, height: 800, m: gridMeasures("expanded", false), widthClass: "expanded", view: "pilot", flies: true, strip: false, gimbal: true, splitRatio: 0.6 });
    expect(big.rightItems).toEqual(["shutter", "mode", "zoom", "gimbal", "settings"]);
  });

  it("keeps the instruments centred on the video", () => {
    const L = flightLayout({ width: 1280, height: 800, m: gridMeasures("expanded", false), widthClass: "expanded", view: "pilot", flies: true, strip: false, gimbal: false, splitRatio: 0.6 });
    expect(L.hud.x + L.hud.w / 2).toBeCloseTo(640);
    expect(L.hud.y + L.hud.h / 2).toBeCloseTo(400);
  });
});

describe("the flight instruments (cockpit spec layers.json)", () => {
  it("draw only at the Instruments level; the centre marks draw at the others but Off", () => {
    for (const level of LAYER_LEVELS) {
      const layers = effectiveLayers(level, {}, "pilot");
      expect(layers.has("instruments"), level).toBe(level === "instruments");
    }
    expect(effectiveLayers("full", {}, "pilot").has("centre_marks")).toBe(true);
  });
});

describe("no legacy chrome on the flight screen (legacy_chrome: none)", () => {
  const src = (p: string) => readFileSync(path.resolve(__dirname, p), "utf8");
  const shell = src("../components/shell/cockpit-shell.tsx");
  const flight = shell.slice(shell.indexOf("if (fullBleed)"), shell.indexOf("data-shell=\"framed\""));

  it("the shell draws no status strip, menu rail or action bar around the Feed", () => {
    expect(flight).toContain('data-shell="flight"');
    for (const legacy of ["<StatusStrip", "<MenuRail", "<ActionBar"]) expect(flight).not.toContain(legacy);
  });

  it("the Feed's grid draws no telemetry strip or hint bar", () => {
    const grid = src("../components/feed/feed-layouts.tsx") + src("../components/screens/feed-screen.tsx");
    for (const legacy of ["StatusStrip", "MenuRail", "ActionBar", "move", "SafetyBand"]) expect(grid).not.toMatch(new RegExp(`<${legacy}\\b`));
  });
});
