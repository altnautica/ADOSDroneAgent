import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

import layersJson from "@/shared/cockpit-spec/layers.json";
import layoutJson from "@/shared/cockpit-spec/layout.json";
import { chromeStyle } from "@/lib/chrome-tokens";
import { gridMeasures, widthClassFor } from "@/lib/cockpit-layout";
import { BUILTIN_LAYERS, COCKPIT_VIEWS, effectiveLayers, LAYER_LEVELS } from "@/lib/cockpit-view";
import {
  barFull,
  CENTRE_MARK_REACH_U,
  CHIP_MAX_FRAC,
  labelPx,
  linkCell,
  linkDetail,
  reserveText,
  severityTextColor,
  TEXT_CRITICAL,
  videoNote,
  classScale,
  FLIGHT_PX,
  flightLayout,
  HIT_AREA,
  intersects,
  movedItems,
  RIGHT_DROP_ORDER,
  TOP_BAR_NEVER_MOVE,
  TOP_BAR_OVERFLOW,
  TOUCH_MIN_PX,
  VIDEO_NOTE_OFFSET_U,
  VIDEO_NOTE_TOP,
  type FlightLayout,
  type Rect,
} from "@/lib/flight-layout";

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
        expect(L.rightItems).toEqual(expect.arrayContaining(["shutter", "photo_video_switch", "camera_settings"]));
        expect(L.bottomRow, `${label}: telemetry row`).not.toBeNull();
        // Every column control's hit area is at least the touch floor.
        expect(L.leftButtonPx, `${label}: left button`).toBeGreaterThanOrEqual(FLIGHT_PX.touchMin);
        // The map view puts the video in the bottom-left card; a split has no card.
        expect(L.cardHoldsVideo, `${label}: video card`).toBe(L.card !== null && label.includes(" map"));
        if (label.includes(" split")) expect(L.card).toBeNull();
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

  it("drops the gimbal pad before zoom where the band is short (right_column.drop_order)", () => {
    expect(RIGHT_DROP_ORDER).toEqual(["gimbal_pad", "zoom"]);
    const m = gridMeasures("medium", false);
    const L = flightLayout({ width: 760, height: 480, m, widthClass: "medium", view: "pilot", flies: true, strip: false, gimbal: true, splitRatio: 0.6 });
    expect(L.rightItems).not.toContain("gimbal_pad");
    const big = flightLayout({ width: 1280, height: 800, m: gridMeasures("expanded", false), widthClass: "expanded", view: "pilot", flies: true, strip: false, gimbal: true, splitRatio: 0.6 });
    expect(big.rightItems).toEqual(["shutter", "photo_video_switch", "zoom", "gimbal_pad", "camera_settings"]);
  });

  it("labels the left column under the icons at medium and up, icons only at compact", () => {
    for (const [w, h, wc, labels] of [
      [580, 360, "compact", false],
      [760, 480, "medium", true],
      [1280, 800, "expanded", true],
      [1920, 1080, "tv", true],
    ] as const) {
      const L = flightLayout({ width: w, height: h, m: gridMeasures(wc, false), widthClass: wc, view: "pilot", flies: true, strip: false, gimbal: false, splitRatio: 0.5 });
      expect(L.leftLabels, wc).toBe(labels);
      const s = gridMeasures(wc, false).scale;
      const item = L.leftButtonPx + (labels ? FLIGHT_PX.leftLabelH : 0);
      expect(L.leftColumn!.h / s).toBeCloseTo(3 * item + 2 * L.leftGapPx);
      if (labels) expect(L.leftColumn!.w / s).toBeGreaterThanOrEqual(FLIGHT_PX.leftLabelW);
    }
  });

  it("gives every right-column control a hit area of at least touch_target.min", () => {
    for (const px of [FLIGHT_PX.modeH, FLIGHT_PX.modeW, FLIGHT_PX.knobHit, (FLIGHT_PX.zoomH - 3) / 2]) expect(px).toBeGreaterThanOrEqual(FLIGHT_PX.touchMin);
    for (const wc of ["compact", "medium", "expanded", "tv"] as const) expect(gridMeasures(wc, false).colPx).toBeGreaterThanOrEqual(FLIGHT_PX.touchMin);
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

/** The top bar's source: the bar and its status pill (`status-pill.tsx`). */
const topBarSource = () =>
  ["top-bar.tsx", "status-pill.tsx"].map((f) => readFileSync(path.resolve(__dirname, "../components/feed/cockpit", f), "utf8")).join("\n");

describe("the top bar's overflow (layout.json top_bar.overflow)", () => {
  it("moves items into the Menu in the spec's order", () => {
    expect(TOP_BAR_OVERFLOW).toEqual(["controller", "nav_source", "link_text", "link_icon", "layer_button"]);
    expect([...movedItems(0)]).toEqual([]);
    expect([...movedItems(3)]).toEqual(["controller", "nav_source", "link_text"]);
    expect(barFull(4)).toBe(false);
    expect(barFull(5)).toBe(true);
  });

  it("never moves the chip, the pill, battery, Menu or STOP", () => {
    expect(TOP_BAR_NEVER_MOVE).toEqual(["flight_state_chip", "status_pill", "battery", "settings", "stop"]);
    for (const id of TOP_BAR_NEVER_MOVE) expect(TOP_BAR_OVERFLOW as readonly string[]).not.toContain(id);
  });

  it("has no link_detail item left in the bar", () => {
    const bar = topBarSource();
    expect(bar).not.toContain("link_detail");
    // The pill truncates with an ellipsis once the bar is full.
    expect(bar).toMatch(/fit \? "min-w-0 shrink"/);
  });
});

describe("the top bar's controls", () => {
  const bar = topBarSource();
  const chip = readFileSync(path.resolve(__dirname, "../components/feed/cockpit/flight-state-chip.tsx"), "utf8");
  const nav = readFileSync(path.resolve(__dirname, "../components/feed/nav-source-chip.tsx"), "utf8");
  const stopButton = bar.slice(bar.indexOf('data-zone="stop"') - 400, bar.indexOf('data-zone="stop"') + 400);

  it("grow every hit area to touch_target.min", () => {
    expect(TOUCH_MIN_PX).toBe(layoutJson.touch_target.min);
    expect(HIT_AREA).toContain(`before:h-[${TOUCH_MIN_PX}px]`);
    expect(HIT_AREA).toContain(`before:w-[max(100%,${TOUCH_MIN_PX}px)]`);
    // The bar is as tall as the hit areas it hosts.
    expect(FLIGHT_PX.barH).toBeGreaterThanOrEqual(TOUCH_MIN_PX);
    // The pill, the link, the layer level and its ▾, Menu and STOP; the chip and its inline button; the nav-source chip.
    expect(bar.match(/cn\(\s*HIT_AREA/g)?.length).toBe(6);
    expect(chip.match(/cn\(HIT_AREA/g)?.length).toBe(2);
    expect(nav).toMatch(/HIT_AREA/);
    // The layer menu's ▾ is itself 48 px wide.
    expect(bar).toMatch(/data-testid="layer-flags-button"\s+className=\{cn\(HIT_AREA, "flex h-\[40px\] w-\[48px\]/);
  });

  it("STOP is never disabled and names its 3 s hold", () => {
    expect(stopButton).not.toMatch(/disabled=/);
    expect(stopButton).toContain('specText("stop.hold_label")');
  });

  it("draw no hard-coded words", () => {
    for (const word of ['"Battery"', "BAT —", '"No pad"', '"Native', '"Not standard"', "`Link ", '"Top bar"', "Emergency stop", '"Flight status"', "Next level"]) {
      expect(bar, word).not.toContain(word);
    }
    for (const word of ["NO PILOT", '"Take control"', '"Resume"', '"Set up aircraft"', "Open mode sheet"]) expect(chip, word).not.toContain(word);
  });

  it("the layer menu lists every layer flag", () => {
    expect(BUILTIN_LAYERS.map((l) => l.id)).toEqual(layersJson.layers.map((l) => l.id));
    expect(BUILTIN_LAYERS.every((l) => l.label.length > 0)).toBe(true);
  });

  it("open the alert list and the layer menu on the shared overlay frame", () => {
    // One overlay at a time; Escape, controller back and a press outside close the newest.
    expect(bar).not.toContain("createPortal");
    expect(bar).not.toContain("useDismiss");
    expect(bar.match(/<Overlay\b/g)?.length).toBe(3);
    expect(bar).toContain('data-testid="status-pill-list"');
  });
});

describe("the no-picture note", () => {
  it("sits below the reticle and the horizon mark's whole travel", () => {
    expect(VIDEO_NOTE_OFFSET_U).toBeGreaterThan(CENTRE_MARK_REACH_U);
    expect(VIDEO_NOTE_TOP).toBe(`calc(50% + ${VIDEO_NOTE_OFFSET_U}rem)`);
    const video = readFileSync(path.resolve(__dirname, "../components/shell/video-layer.tsx"), "utf8");
    expect(video).toContain("top: VIDEO_NOTE_TOP");
    // Never the transport's error text, never a claim that no source exists.
    expect(video).not.toContain("snapshot.error");
    expect(video).not.toContain("No video source");
  });

  it("is sized in px × the class scale", () => {
    expect(classScale("tv")).toBe(layoutJson.classes.tv.scale);
    expect(classScale("medium")).toBe(1);
  });
});

describe("the top bar's cells from the spec", () => {
  const bar = readFileSync(path.resolve(__dirname, "../components/feed/cockpit/top-bar.tsx"), "utf8");
  const chip = readFileSync(path.resolve(__dirname, "../components/feed/cockpit/flight-state-chip.tsx"), "utf8");

  it("the link cell reads the frame age and colours its icon by 3000/10000 ms", () => {
    expect(linkCell(420)).toEqual({ text: "0.4 s", severity: "ok" });
    expect(linkCell(2999).severity).toBe("ok");
    expect(linkCell(3000)).toEqual({ text: "3.0 s", severity: "warning" });
    expect(linkCell(9999).severity).toBe("warning");
    expect(linkCell(12_400)).toEqual({ text: "12 s", severity: "critical" });
    expect(linkCell(null)).toEqual({ text: "No link", severity: "critical" });
    expect(linkDetail(-71.6)).toBe("RSSI -72 dBm");
    expect(linkDetail(undefined)).toBeNull();
    expect(bar).not.toContain("linkLabel");
    expect(bar).toContain('data-testid="link-popover"');
  });

  it("battery reads its time to the reserve, short while the bar is short", () => {
    expect(reserveText("4:05", false)).toBe("4:05 to reserve");
    expect(reserveText("4:05", true)).toBe("4:05 RES");
    expect(bar).toContain("reserveText(fmtClock(r.timeToReserveS), compact)");
  });

  it("critical text, panel and STOP colours come from the chrome tokens", () => {
    expect(TEXT_CRITICAL).toBe("#FF8A80");
    expect(severityTextColor("critical")).toBe(TEXT_CRITICAL);
    expect(chromeStyle()).toMatchObject({ "--fs-panel-bg": "#080A0EC8", "--fs-stop-bg": "#C81E1E" });
    const stack = readFileSync(path.resolve(__dirname, "../components/feed/alert-stack.tsx"), "utf8");
    expect(stack).toContain("TEXT_CRITICAL");
  });

  it("a tv label is never below label_min_px", () => {
    expect(labelPx(11, "tv")).toBe(16);
    expect(labelPx(14, "tv")).toBeCloseTo(14 * 1.33);
    expect(labelPx(11, "medium")).toBe(11);
  });

  it("the alert stack and the nav-source popover draw their words from the spec", () => {
    const stack = readFileSync(path.resolve(__dirname, "../components/feed/alert-stack.tsx"), "utf8");
    expect(stack).toContain("alert.level.");
    for (const w of ['"Critical"', '"Warning"', '"Advisory"']) expect(stack, w).not.toContain(w);
    const pop = readFileSync(path.resolve(__dirname, "../components/feed/nav-source-popover.tsx"), "utf8");
    for (const w of ['"ACTIVE"', ">Set<", ">Ext nav<", ">GPS<", ">EKF<", "Use set {s}", "refused`", "sats`", "No live flight controller", "No acknowledgement"]) {
      expect(pop, w).not.toContain(w);
    }
    expect(pop).toContain('specText("nav_source.no_ack")');
  });

  it("the chip is capped at max_frac of the bar with its full text on hover and tap", () => {
    expect(CHIP_MAX_FRAC).toBe(0.4);
    expect(chip).toContain("${CHIP_MAX_FRAC * 100}cqw");
    expect(bar).toContain('containerType: "inline-size"');
    expect(chip).toContain("title={full}");
    expect(chip).toContain('data-testid="flight-state-chip-full"');
  });
});

describe("the no-picture note's words", () => {
  it("come from video.state with a retry cue", () => {
    expect(videoNote("connecting")).toEqual({ text: "Connecting to video…", retry: false });
    expect(videoNote("failed")).toEqual({ text: "Video failed", retry: true });
    expect(videoNote("frozen")).toEqual({ text: "Picture frozen", retry: true });
    expect(videoNote("live")).toBeNull();
    expect(videoNote("connecting", false)).toEqual({ text: "No video source", retry: false });
    const video = readFileSync(path.resolve(__dirname, "../components/shell/video-layer.tsx"), "utf8");
    expect(video).not.toContain("Connecting to feed");
    expect(video).toContain('specText("video.state.retry")');
  });
});
