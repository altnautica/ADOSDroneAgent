// @vitest-environment happy-dom
// The minimap and HUD controls: a minimap mounted while the fix has been low
// for a while opens in the local frame at once; the card's north label sits
// below the card title; every small control has a 48 px hit area; the column
// captions sit on a panel plate; the heading readout carries a halo; and the
// nav-source popover names the pose age.

import { readFileSync } from "node:fs";
import path from "node:path";
import { createElement } from "react";
import { act } from "react-dom/test-utils";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it } from "vitest";

import { MiniMap } from "@/components/feed/mini-map";
import { drawCentreMarks, HEADING_HALO, HEADING_HALO_PX } from "@/components/feed/hud-centre-marks";
import { extNavLine } from "@/lib/ekf-sources";
import { CARD_TITLE_BOTTOM_PX, northLabelY, svgTextUnits } from "@/lib/minimap-geometry";
import type { VehicleState } from "@/lib/types";
import tokensJson from "@/shared/cockpit-spec/tokens.json";
import { specText } from "@/shared/hud/spec-text";
import { INITIAL_FLIGHT_STATE, useFlightStore } from "@/stores/flight-store";

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

const src = (p: string) => readFileSync(path.resolve(__dirname, p), "utf8");

let host: HTMLDivElement | null = null;
let root: Root;

afterEach(async () => {
  if (host) {
    await act(async () => root.unmount());
    host.remove();
    host = null;
  }
  useFlightStore.setState({ ...INITIAL_FLIGHT_STATE });
});

describe("the minimap frame across a remount", () => {
  it("a minimap mounted after 3 s below a 3D fix opens in the local frame", async () => {
    useFlightStore.setState({ live: true, telemetry: { gps: { fix_type: 1 } } as VehicleState, noFixSince: Date.now() - 3000 });
    host = document.createElement("div");
    document.body.appendChild(host);
    root = createRoot(host);
    await act(async () => root.render(createElement(MiniMap)));
    expect(host.querySelector('[data-testid="minimap-header"]')?.textContent).toContain(specText("minimap.local_no_gps"));
  });
});

describe("the minimap card's north label", () => {
  it("sits below the card title in a card, near the top elsewhere", () => {
    const box = 120;
    const t = svgTextUnits(11, box, 100);
    expect(northLabelY(t, false, box, 100)).toBeCloseTo(t * 1.2);
    const inCard = northLabelY(t, true, box, 100);
    // Its glyph top (baseline less one text height) is below the title.
    expect(inCard - t).toBeGreaterThanOrEqual(svgTextUnits(CARD_TITLE_BOTTOM_PX, box, 100) - 1e-9);
  });
});

describe("small map and HUD controls", () => {
  it("each has a 48 px hit area", () => {
    const map = src("./mini-map.tsx");
    expect(map.match(/cn\(HIT_AREA/g)?.length).toBe(2);
    const local = src("./local-mini-map.tsx");
    expect(local.match(/cn\(HIT_AREA/g)?.length).toBe(2);
    expect(local).toContain("gap-[20px]");
    expect(src("./hud-canvas.tsx")).toContain('cn(HIT_AREA, "chrome-panel pointer-events-auto absolute');
  });

  it("column captions sit on a panel plate in the text colour", () => {
    const button = src("./cockpit/zone-button.tsx");
    const caption = button.slice(button.indexOf("A panel plate under the caption"), button.indexOf('data-testid="zone-caption"'));
    expect(caption).toContain('className="chrome-panel');
    expect(caption).toContain('color: "var(--fs-text)"');
    expect(caption).not.toContain("chrome-label");
  });

  it("the heading readout is stroked with the halo under its glyphs", () => {
    const calls: string[] = [];
    const ctx = new Proxy(
      {},
      {
        get: (_t, k: string) =>
          ["save", "restore", "beginPath", "moveTo", "lineTo", "stroke", "translate", "rotate", "arc", "fill", "closePath"].includes(k)
            ? () => undefined
            : k === "strokeText" || k === "fillText"
              ? (text: string) => calls.push(`${k}:${text}`)
              : undefined,
        set: (_t, k: string, v) => {
          if (k === "strokeStyle" || k === "lineWidth") calls.push(`${k}=${v}`);
          return true;
        },
      },
    ) as CanvasRenderingContext2D;
    drawCentreMarks(ctx, 100, 100, 20, { primary: "#fff", ink: "#fff", ink2: "#ccc", glass: "#000" }, null, null, 43, () => "12px sans-serif");
    expect(HEADING_HALO).toBe(tokensJson.ar.halo);
    const stroke = calls.indexOf("strokeText:043°");
    expect(stroke).toBeGreaterThan(-1);
    expect(calls.slice(0, stroke)).toContain(`strokeStyle=${HEADING_HALO}`);
    expect(calls.slice(0, stroke)).toContain(`lineWidth=${HEADING_HALO_PX}`);
    expect(calls.indexOf("fillText:043°")).toBeGreaterThan(stroke);
  });
});

describe("the nav-source popover", () => {
  it("names the pose age while vision-nav reports one", () => {
    expect(extNavLine({ extnavHealth: "good", odomRateHz: 30, poseAgeMs: 42.4 })).toBe(`GOOD · 30.0 Hz · ${specText("nav_source.pose_age", { ms: 42 })}`);
    expect(extNavLine({ extnavHealth: "good", odomRateHz: null, poseAgeMs: null })).toBe("GOOD");
    expect(extNavLine(null)).toBe("—");
  });
});
